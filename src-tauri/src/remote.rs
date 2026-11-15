//! Asks the server which shares it offers, over SMB itself. This works with
//! any account that can log in to the shares, not just a HexOS admin.

use std::time::Duration;

pub struct RemoteShare {
    pub name: String,
    pub comment: String,
}

const TIMEOUT: Duration = Duration::from_secs(20);

pub async fn list_shares(
    host: &str,
    username: &str,
    password: &str,
) -> Result<Vec<RemoteShare>, String> {
    match tokio::time::timeout(TIMEOUT, query(host, username, password)).await {
        Ok(result) => result,
        Err(_) => Err(format!("{host} did not respond")),
    }
}

async fn query(host: &str, username: &str, password: &str) -> Result<Vec<RemoteShare>, String> {
    let client = smb::Client::new(smb::ClientConfig::default());
    client
        .ipc_connect(host, username, password.to_string())
        .await
        .map_err(|e| describe(host, e))?;
    let result = client.list_shares(host).await.map_err(|e| describe(host, e));
    let _ = client.close().await;

    let mut shares: Vec<RemoteShare> = result?
        .iter()
        .filter(|info| info.share_type.kind() == smb_rpc::interface::ShareKind::Disk)
        .filter(|info| !info.share_type.special())
        .filter_map(|info| {
            let name = ndr_text(&info.netname)?;
            // Hidden administrative shares (C$, ADMIN$, ...).
            if name.is_empty() || name.ends_with('$') {
                return None;
            }
            Some(RemoteShare {
                name,
                comment: ndr_text(&info.remark).unwrap_or_default(),
            })
        })
        .collect();
    shares.sort_by_key(|s| s.name.to_lowercase());
    Ok(shares)
}

fn ndr_text(
    ptr: &smb_rpc::ndr64::NdrPtr<smb_rpc::ndr64::NdrString<u16>>,
) -> Option<String> {
    let value = ptr.as_ref()?;
    Some((**value).to_string().trim_end_matches('\0').to_string())
}

fn describe(host: &str, e: smb::Error) -> String {
    let text = e.to_string();
    let lower = text.to_lowercase();
    if lower.contains("logon") || lower.contains("access denied") || lower.contains("password") {
        "The server rejected the username or password.".into()
    } else if lower.contains("refused")
        || lower.contains("unreachable")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("lookup")
    {
        format!("Could not reach {host}.")
    } else {
        format!("Could not list shares on {host}: {text}")
    }
}
