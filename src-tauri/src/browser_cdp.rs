//! Local browser-control protocol. A confirmed CDP endpoint is shared by
//! normal shutdown, crash recovery, and the automation API.
use anyhow::{Context, Result};
use futures_util::SinkExt;
use std::path::Path;
use std::time::Duration;

pub async fn discover(directory: &Path) -> Result<Option<crate::process::CdpInfo>> {
    let marker = match tokio::fs::read_to_string(directory.join("DevToolsActivePort")).await {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read browser debugging endpoint"),
    };
    let mut lines = marker.lines();
    let port: u16 = lines
        .next()
        .context("missing browser debugging port")?
        .trim()
        .parse()?;
    let path = lines
        .next()
        .context("missing browser debugging path")?
        .trim();
    anyhow::ensure!(
        port != 0 && path.starts_with("/devtools/browser/"),
        "Invalid browser debugging endpoint"
    );
    let http_url = format!("http://127.0.0.1:{port}");
    let expected = format!("ws://127.0.0.1:{port}{path}");
    let version: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?
        .get(format!("{http_url}/json/version"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        version["webSocketDebuggerUrl"].as_str() == Some(&expected),
        "Browser debugging endpoint does not match its marker"
    );
    Ok(Some(crate::process::CdpInfo {
        port,
        http_url,
        web_socket_debugger_url: expected,
    }))
}

pub async fn close(endpoint: &crate::process::CdpInfo) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut socket, _) =
            tokio_tungstenite::connect_async(&endpoint.web_socket_debugger_url).await?;
        socket
            .send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"id":1,"method":"Browser.close"}"#,
            ))
            .await?;
        // Successful transmission is only a request. The process tracker waits
        // for the actual process exit before reporting a completed shutdown.
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Browser close request timed out")?
}
