use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);
pub const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_MSG_SIZE: usize = 10 * 1024 * 1024;
pub const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const PUMPFUN_MINT_AUTHORITY: &str = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM";