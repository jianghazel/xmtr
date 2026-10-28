use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ureq::Agent;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::dns::Dns;
use crate::model::{Problem, Upload, UploadAccepted};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Hands ureq addresses that were resolved with the resolver chosen by `--dns`.
#[derive(Debug)]
struct PinnedResolver(Vec<SocketAddr>);

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addrs = self.empty();
        for addr in self.0.iter().take(16) {
            addrs.push(*addr);
        }
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(addrs)
    }
}

fn agent(url: &str, dns: &Dns) -> Result<Agent> {
    let config = Agent::config_builder()
        .timeout_global(Some(UPLOAD_TIMEOUT))
        .http_status_as_error(false)
        // Addresses are pinned to the gateway host, so a redirect elsewhere would reach the wrong server.
        .max_redirects(0)
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build();
    if dns.uses_system() {
        return Ok(config.into());
    }
    let uri: Uri = url.parse().with_context(|| format!("parsing {url}"))?;
    let host = uri.host().context("gateway URL has no host")?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("http") {
            80
        } else {
            443
        });
    let addrs = dns
        .resolve(host)?
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    Ok(Agent::with_parts(
        config,
        DefaultConnector::new(),
        PinnedResolver(addrs),
    ))
}

/// Sends the routes to the gateway and returns the stored trace ID, view key and IPMap link.
pub fn upload(api: &str, body: &Upload, dns: &Dns) -> Result<UploadAccepted> {
    let url = format!("{}/api/v1/traces", api.trim_end_matches('/'));
    let mut response = agent(&url, dns)?
        .post(&url)
        .send_json(body)
        .with_context(|| format!("uploading to {url}"))?;
    let status = response.status();
    if status.is_success() {
        return response
            .body_mut()
            .read_json::<UploadAccepted>()
            .context("reading upload response");
    }
    let problem = response.body_mut().read_json::<Problem>().ok();
    let reason = problem
        .and_then(|problem| problem.detail.or(problem.title))
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("unknown error")
                .to_owned()
        });
    bail!("upload rejected with HTTP {}: {reason}", status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_resolver_accepts_an_ipv6_gateway_literal() {
        let dns = Dns::new(&"127.0.0.1:9".parse().unwrap()).unwrap();
        agent("http://[::1]:8080/api/v1/traces", &dns).unwrap();
        agent("http://127.0.0.1:8080/api/v1/traces", &dns).unwrap();
    }
}
