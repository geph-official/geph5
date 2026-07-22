//! One switch, full engine-startup timeline. Localizes the ~12s reconcile stall:
//! prints wall-clock (UTC epoch ms) at apply start/return, then dumps the new
//! engine's entire recent-log ring so we can see when it first logged and where
//! any multi-second gap sits.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use geph5_broker_protocol::ExitConstraint;
use geph5_misc_rpc::manager_control::{self, SessionContext, TunnelSettings};
use isocountry::CountryCode;

async fn ctl<T, E: std::fmt::Debug>(
    fut: impl std::future::Future<Output = Result<Result<T, String>, E>>,
) -> anyhow::Result<T> {
    match fut.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(s)) => anyhow::bail!("manager error: {s}"),
        Err(e) => anyhow::bail!("transport error: {e:?}"),
    }
}

fn epoch_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

fn main() -> anyhow::Result<()> {
    geph5_rt::block_on(async move {
        let client = manager_control::manager_control_client();
        let view = ctl(client.get_settings()).await?;
        let base: TunnelSettings = view.tunnel_settings();
        let exits = ctl(client.list_exits()).await?;

        // Current city, to pick a different target.
        let current_city = match &base.exit_constraint {
            ExitConstraint::CountryCity(_, c) => c.clone(),
            _ => String::new(),
        };
        let target = exits
            .iter()
            .find(|e| e.city != current_city)
            .ok_or_else(|| anyhow::anyhow!("no alternate exit"))?;
        let cc = CountryCode::for_alpha2(&target.country).unwrap();
        println!("switching to {}/{}", target.country, target.city);

        let mut settings = base.clone();
        settings.exit_constraint = ExitConstraint::CountryCity(cc, target.city.clone());

        let start_wall = epoch_ms();
        let t0 = Instant::now();
        println!("APPLY_START  epoch_ms={start_wall}");
        ctl(client.apply_settings(settings, SessionContext::default())).await?;
        let apply = t0.elapsed();
        let ret_wall = epoch_ms();
        println!("APPLY_RETURN epoch_ms={ret_wall}  (+{:.3}s)", apply.as_secs_f64());

        // Grab the engine's whole ring immediately.
        let logs = ctl(client.logs(5000)).await?;
        let grab_wall = epoch_ms();
        println!("LOGS_GRABBED epoch_ms={grab_wall}  ({} lines)", logs.len());
        println!("--- reference: APPLY_START in UTC was {} ms after epoch ---", start_wall);
        println!("--- (engine log timestamps are ISO-8601 Z; compare to APPLY_START/RETURN) ---");

        // Print only messages + timestamps, trimmed, to keep it readable, and flag
        // gaps >= 1s between consecutive engine log lines.
        let mut prev: Option<f64> = None;
        for line in &logs {
            // crude parse: timestamp is the value after "timestamp":"
            let ts = line
                .split("\"timestamp\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("");
            // seconds-in-day for gap detection: parse HH:MM:SS.fff out of ...THH:MM:SS.ffffffZ
            let secs = ts
                .split('T')
                .nth(1)
                .map(|hms| {
                    let hms = hms.trim_end_matches('Z');
                    let mut it = hms.split(':');
                    let h: f64 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                    let m: f64 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                    let s: f64 = it.next().unwrap_or("0").parse().unwrap_or(0.0);
                    h * 3600.0 + m * 60.0 + s
                });
            let msg = line
                .split("\"message\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or(line);
            let target = line
                .split("\"target\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("");
            if let Some(s) = secs {
                let gap = prev.map(|p| s - p).unwrap_or(0.0);
                let flag = if gap >= 1.0 { format!("  <<< GAP {gap:.2}s") } else { String::new() };
                println!("{ts}  [{target}] {msg}{flag}");
                prev = Some(s);
            } else {
                println!("(no-ts) {msg}");
            }
        }

        // restore
        let _ = ctl(client.apply_settings(base, SessionContext::default())).await;
        let _ = Duration::from_secs(0);
        anyhow::Ok(())
    })?;
    Ok(())
}
