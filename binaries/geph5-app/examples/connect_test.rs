//! Trigger a connect via the manager control pipe and surface the result + engine
//! logs, to test whether the current binaries can bring up the VPN tunnel.

use std::time::Duration;

use geph5_misc_rpc::manager_control::{self, ConnState, SessionContext};

async fn ctl<T, E: std::fmt::Debug>(
    fut: impl std::future::Future<Output = Result<Result<T, String>, E>>,
) -> anyhow::Result<T> {
    match fut.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(s)) => anyhow::bail!("manager error: {s}"),
        Err(e) => anyhow::bail!("transport error: {e:?}"),
    }
}

fn main() -> anyhow::Result<()> {
    geph5_rt::block_on(async move {
        let client = manager_control::manager_control_client();
        let view = ctl(client.get_settings()).await?;
        println!(
            "BEFORE: logged_in={} connected={} vpn={} exit={:?}",
            view.logged_in, view.connected, view.vpn, view.exit_constraint
        );

        println!("calling connect()...");
        let t0 = std::time::Instant::now();
        match client.connect(SessionContext::default()).await {
            Ok(Ok(())) => println!("connect() OK in {:.2}s", t0.elapsed().as_secs_f64()),
            Ok(Err(e)) => println!("connect() MANAGER ERROR after {:.2}s: {e}", t0.elapsed().as_secs_f64()),
            Err(e) => println!("connect() TRANSPORT ERROR: {e:?}"),
        }

        let mut last = String::new();
        for _ in 0..40 {
            match ctl(client.status()).await {
                Ok(st) => {
                    let s = format!("{:?}", st.state);
                    if s != last {
                        println!("status: {s} (t+{:.1}s)", t0.elapsed().as_secs_f64());
                        last = s;
                    }
                    if matches!(st.state, ConnState::Connected) {
                        println!("=> CONNECTED at t+{:.2}s", t0.elapsed().as_secs_f64());
                        break;
                    }
                }
                Err(e) => println!("status err: {e:#}"),
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        println!("--- recent engine logs ---");
        match ctl(client.logs(60)).await {
            Ok(logs) => {
                for l in logs {
                    println!("| {}", l.trim_end());
                }
            }
            Err(e) => println!("(logs error: {e:#})"),
        }
        anyhow::Ok(())
    })?;
    Ok(())
}
