use std::fmt::Display;

#[derive(Debug)]
pub enum Error {
    InvalidSlabSize,
    InvalidNumWorkers,
    InvalidWorkerIndex,
    InvalidFileSize,
    AlreadyInitialized,
    InvalidHeader,
    IoError(std::io::Error),
    MmapError(std::io::Error),
}

impl std::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSlabSize => write!(f, "invalid slab size"),
            Self::InvalidNumWorkers => write!(f, "invalid num workers"),
            Self::InvalidWorkerIndex => write!(f, "invalid worker index"),
            Self::InvalidFileSize => write!(f, "invalid file size"),
            Self::AlreadyInitialized => write!(f, "already initialized"),
            Self::InvalidHeader => write!(f, "invalid header"),
            Self::IoError(err) => write!(f, "io error; err={err}"),
            Self::MmapError(err) => write!(f, "mmap error; err={err}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}
