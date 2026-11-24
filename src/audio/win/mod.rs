//! Windows audio backends.  ASIO owns the low-latency side; WASAPI is the
//! system host and also provides REAL-style shared and exclusive clock paths.
//! `output` decides which of them the clock output opens, and in what order.

pub(crate) mod asio;
pub(crate) mod output;
pub(crate) mod wasapi;
