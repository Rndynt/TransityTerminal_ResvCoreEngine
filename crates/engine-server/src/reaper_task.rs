use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info};

use crate::state::AppState;

pub async fn run(state: AppState, interval_secs: u64, confirmed_retention_days: i64) {
    let mut tick = interval(Duration::from_secs(interval_secs.max(1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    info!(
        interval_secs,
        confirmed_retention_days, "reaper task started"
    );
    loop {
        tick.tick().await;
        match engine_core::expire_holds(
            &state.pool,
            &*state.publisher,
            confirmed_retention_days,
        )
        .await
        {
            Ok(r) if r.released_count > 0 => {
                info!(released = r.released_count, "expired holds released");
            }
            Ok(_) => {}
            Err(e) => {
                error!(error = %e, "reaper iteration failed");
            }
        }
    }
}
