//! Clocks are injectable without coupling room logic to HTTP or Tokio time.

use std::sync::Arc;
use time::OffsetDateTime;

pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(OffsetDateTime::now_utc)
}
