#[derive(Debug)]
pub enum Error {
    InvalidSlabSize,
    InvalidNumWorkers,
    InvalidWorkerIndex,
    InvalidFileSize,
    AlreadyInitialized,
    InvalidHeader,
    IoError(std::io::Error),
    MMapError(std::io::Error),
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}
