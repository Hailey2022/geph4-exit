use std::{sync::Arc, time::Duration};

use smol::{
    io::{AsyncRead, AsyncWrite},
    net::AsyncToSocketAddrs,
};
use smol_timeout::TimeoutExt;

use crate::{listen::RootCtx, ratelimit::RateLimiter};

/// Connects to a remote host and forwards traffic to/from it and a given client.
pub async fn proxy_loop(
    ctx: Arc<RootCtx>,
    rate_limit: Arc<RateLimiter>,
    client: impl AsyncRead + AsyncWrite + Clone + Unpin,
    addr: impl AsyncToSocketAddrs,
    count_stats: bool,
) -> anyhow::Result<()> {
    // Incr/decr the connection count
    ctx.conn_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _deferred = scopeguard::guard((), |_| {
        ctx.conn_count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    });

    let addr = smol::net::resolve(addr)
        .await?
        .into_iter()
        .find(|v| v.is_ipv4())
        .ok_or_else(|| anyhow::anyhow!("no IPv4 address"))?;
    let asn = crate::asn::get_asn(addr.ip());
    log::trace!(
        "got connection request to AS{} (conn_count = {})",
        asn,
        ctx.conn_count.load(std::sync::atomic::Ordering::Relaxed)
    );
    let key = format!("exit_usage.{}", ctx.exit_hostname.replace(".", "-"));
    // log::debug!("helper got destination {} (AS{})", addr, asn);

    if crate::lists::BLACK_PORTS.contains(&addr.port()) {
        anyhow::bail!("port blacklisted")
    }
    if ctx.port_whitelist && !crate::lists::WHITE_PORTS.contains(&addr.port()) {
        anyhow::bail!("port not whitelisted")
    }

    let to_conn = if let Some(proxy) = ctx.google_proxy {
        if addr.port() == 443 && asn == crate::asn::GOOGLE_ASN {
            proxy
        } else {
            addr
        }
    } else {
        addr
    };

    let remote = smol::net::TcpStream::connect(to_conn)
        .timeout(Duration::from_secs(60))
        .await
        .ok_or_else(|| anyhow::anyhow!("connect timed out"))??;
    remote.set_nodelay(true)?;
    let remote2 = remote.clone();
    let client2 = client.clone();
    smol::future::race(
        geph4_aioutils::copy_with_stats_async(remote2, client2, |n| {
            if fastrand::f32() < 0.01 && count_stats {
                ctx.stat_client.count(&key, n as f64 * 100.0);
            }
            let rate_limit = rate_limit.clone();
            async move {
                rate_limit.wait(n).await;
            }
        }),
        geph4_aioutils::copy_with_stats(client, remote, |n| {
            if fastrand::f32() < 0.01 && count_stats {
                ctx.stat_client.count(&key, n as f64 * 100.0);
            }
        }),
    )
    .await?;
    Ok(())
}
