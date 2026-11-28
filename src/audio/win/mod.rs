//! ASIO is low latency.  WASAPI is the system host, plus REAL-style shared
//! and exclusive clock paths.  `output` picks which opens, and in what order.

pub(crate) mod asio;
pub(crate) mod output;
pub(crate) mod wasapi;
