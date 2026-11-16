//! Clocks are injectable without coupling room logic to HTTP or Tokio time.

use chrono::{DateTime, Utc};
use std::sync::Arc;

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}
