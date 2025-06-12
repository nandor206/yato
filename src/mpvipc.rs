// =============== Imports ================
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Duration, Instant, sleep, timeout};

#[derive(Serialize)]
#[serde(untagged)]
enum MPVCommand<'a> {
    Get { command: [&'a str; 2] },
    Seek { command: [&'a str; 3] },
}

#[derive(Deserialize, Debug)]
struct MPVResponse {
    data: Option<f64>,
    error: String,
}

pub async fn get_property(property: &str) -> Result<f64> {
    use tokio::net::UnixStream;

    let stream = UnixStream::connect("/tmp/yato-mpvsocket")
        .await
        .context("Failed to connect to MPV socket")?;

    let cmd = MPVCommand::Get {
        command: ["get_property", property],
    };
    let json = serde_json::to_string(&cmd)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;

    let mut response = String::new();
    reader.read_line(&mut response).await?;

    let parsed: MPVResponse = serde_json::from_str(&response)?;

    if parsed.error == "success" {
        if let Some(val) = parsed.data {
            Ok(val)
        } else {
            Err(anyhow::anyhow!("MPV response missing 'data' field"))
        }
    } else {
        Err(anyhow::anyhow!("MPV IPC error: {}", parsed.error))
    }
}

// Send a `seek` command to MPV to seek to a specific time
pub async fn seek_to(time: f64) -> Result<()> {
    let mut stream = UnixStream::connect("/tmp/yato-mpvsocket")
        .await
        .context("Failed to connect to MPV socket")?;

    let time_str = time.to_string();
    let cmd = MPVCommand::Seek {
        command: ["seek", &time_str, "absolute"],
    };

    let json =
        serde_json::to_string(&cmd).with_context(|| "Failed to serialize seek command to JSON")?;
    stream.write_all(format!("{}\n", json).as_bytes()).await?;

    Ok(())
}

pub async fn send_command(command: &[&str]) -> Result<Value, Box<dyn std::error::Error>> {
    let max_retries = 3;
    let retry_delay = Duration::from_millis(100);

    for attempt in 0..max_retries {
        if attempt > 0 {
            sleep(retry_delay).await;
            log::warn!(
                "Retrying MPV command, attempt {}/{}",
                attempt + 1,
                max_retries
            );
        }

        // Connect to the socket
        let stream = match UnixStream::connect("/tmp/yato-mpvsocket").await {
            Ok(s) => s,
            Err(err) => {
                log::error!("Connect error (attempt {}/{}): {}", attempt + 1, max_retries, err);
                continue;
            }
        };

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut response_line = String::new();

        // Send command
        let command_str = json!({ "command": command }).to_string() + "\n";
        if let Err(err) = write_half.write_all(command_str.as_bytes()).await {
            log::error!("Write error (attempt {}/{}): {}", attempt + 1, max_retries, err);
            continue;
        }

        // Read single line response
        match reader.read_line(&mut response_line).await {
            Ok(0) => {
                log::error!("No response received (attempt {}/{})", attempt + 1, max_retries);
                continue;
            }
            Ok(_) => {
                match serde_json::from_str::<Value>(&response_line) {
                    Ok(json) => {
                        if json.get("error") == Some(&Value::String("success".into())) {
                            return Ok(json.get("data").cloned().unwrap_or(Value::Null));
                        } else {
                            return Err(format!(
                                "MPV returned error: {}",
                                json.get("error").unwrap_or(&Value::String("unknown".into()))
                            )
                            .into());
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to parse JSON: {}", err);
                        continue;
                    }
                }
            }
            Err(err) => {
                log::error!("Read error (attempt {}/{}): {}", attempt + 1, max_retries, err);
                continue;
            }
        }
    }

    Err(format!("MPV command failed after {} attempts", max_retries).into())
}

pub async fn set_property(name: &str, value: &str) -> Result<()> {
    let mut stream = UnixStream::connect("/tmp/yato-mpvsocket")
        .await
        .context("Failed to connect to MPV socket")?;

    let msg = json!({
        "command": ["set_property", name, value],
    });
    let msg_str = format!("{}\n", msg.to_string());

    stream
        .write_all(msg_str.as_bytes())
        .await
        .context("Failed to write command to MPV socket")?;
    Ok(())
}

pub async fn exit_mpv() -> Result<()> {
    // Send command to close MPV
    match send_command(&["quit"]).await {
        Ok(_) => Ok(()),
        Err(err) => {
            log::error!("Error closing MPV: {}", err);
            Err(anyhow::anyhow!("{}", err))
        }
    }
}

pub async fn is_mpv_idle() -> bool {
    let msg = r#"{ "command": ["get_property", "idle-active"] }\n"#;

    // Try connecting to MPV socket
    if let Ok(stream) = UnixStream::connect("/tmp/yato-mpvsocket").await {
        let (read_half, mut writer) = stream.into_split();
        writer.write_all(msg.as_bytes()).await.ok().unwrap();

        // Setup reader
        let mut reader = BufReader::new(read_half);
        let mut response = String::new();

        // Try reading within the timeout
        if let Ok(Ok(n)) = timeout(Duration::from_millis(100), reader.read_line(&mut response)).await {
            if n > 0 {
                if let Ok(json) = serde_json::from_str::<Value>(&response) {
                    // Check for success and extract the idle state
                    if json.get("error").and_then(|e| e.as_str()) == Some("success") {
                        return json.get("data").and_then(|d| d.as_bool()).unwrap_or(false);
                    }
                }
            }
        }
    }

    // Default to false if something goes wrong
    false
}

pub async fn is_mpv_eof() -> bool {
    let msg = r#"{ "command": ["get_property", "eof-reached"] }\n"#;

    // Try connecting to MPV socket
    if let Ok(stream) = UnixStream::connect("/tmp/yato-mpvsocket").await {
        let (read_half, mut writer) = stream.into_split();
        writer.write_all(msg.as_bytes()).await.ok().unwrap();

        // Setup reader
        let mut reader = BufReader::new(read_half);
        let mut response = String::new();

        // Try reading within the timeout
        if let Ok(Ok(n)) = timeout(Duration::from_millis(100), reader.read_line(&mut response)).await {
            if n > 0 {
                if let Ok(json) = serde_json::from_str::<Value>(&response) {
                    // Check for success and extract the eof-reached state
                    if json.get("error").and_then(|e| e.as_str()) == Some("success") {
                        return json.get("data").and_then(|d| d.as_bool()).unwrap_or(false);
                    }
                }
            }
        }
    }

    // Default to false if something goes wrong
    false
}


pub async fn something_is_on() -> bool {
    let msg = r#"{ "command": ["get_property", "path"] }\n"#;

    for _ in 0..3 {
        if let Ok(stream) = UnixStream::connect("/tmp/yato-mpvsocket").await {
            let (read_half, mut writer) = stream.into_split();
            if writer.write_all(msg.as_bytes()).await.is_ok() {
                let mut reader = BufReader::new(read_half);
                let mut response = String::new();
                if reader.read_line(&mut response).await.is_ok() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&response) {
                        if json["error"] == "success" {
                            return true;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

pub async fn has_active_playback() -> Result<bool> {
    let max_retries = 3;
    let msg = json!({ "command": ["get_property", "time-pos"] }).to_string() + "\n";
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..max_retries {
        if attempt > 0 {
            sleep(Duration::from_millis(100)).await;
        }

        match UnixStream::connect("/tmp/yato-mpvsocket").await {
            Ok(stream) => {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut response = String::new();

                if let Err(e) = write_half.write_all(msg.as_bytes()).await {
                    return Err(anyhow::anyhow!(e));
                }

                match reader.read_line(&mut response).await {
                    Ok(_) => {
                        match serde_json::from_str::<Value>(&response) {
                            Ok(json) => {
                                if json["error"] == "success" {
                                    // Got time-pos, so something is playing
                                    return Ok(true);
                                } else if json["error"] == "property unavailable" {
                                    return Ok(false);
                                } else {
                                    return Err(anyhow::anyhow!("Unexpected error: {}", json["error"]));
                                }
                            }
                            Err(e) => return Err(anyhow::anyhow!(e)),
                        }
                    }
                    Err(e) => return Err(anyhow::anyhow!(e)),
                }
            }
            Err(e) => {
                let err_msg = e.to_string();
                if err_msg.contains("No such file") || err_msg.contains("Connection refused") {
                    last_err = Some(anyhow::anyhow!(e));
                    continue;
                } else {
                    return Err(anyhow::anyhow!(e));
                }
            }
        }
    }

    Err(last_err.unwrap_or(anyhow::anyhow!("unknown error")))
}

pub async fn get_mpv_pause_status() -> Result<bool, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect("/tmp/yato-mpvsocket").await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    
    let cmd = json!({
        "command": ["get_property", "pause"]
    });

    // Send command
    writer.write_all(cmd.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;

    let mut response = String::new();
    reader.read_line(&mut response).await?;

    let value = serde_json::from_str::<serde_json::Value>(&response)?;

    if value["error"] == "success" {
        Ok(value["data"].as_bool().unwrap_or(false))
    } else {
        Ok(false)
    }
}