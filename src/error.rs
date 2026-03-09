use std::fmt::Display;

#[derive(Debug)]
pub enum Error {
    InvalidMagic,
    InvalidVersion { expected: u32, actual: u32 },
    InvalidSlabSize,
    InvalidNumWorkers,
    InvalidWorkerIndex,
    HeaderMismatch,
    NoAvailableWorkers,
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
            Self::InvalidMagic => write!(f, "invalid magic"),
            Self::InvalidVersion { expected, actual } => write!(
                f,
                "invalid version; expected={}.{}; found={}.{}",
                expected >> 16,
                expected & 0xFFFF,
                actual >> 16,
                actual & 0xFFFF,
            ),
            Self::InvalidSlabSize => write!(f, "invalid slab size"),
            Self::InvalidNumWorkers => write!(f, "invalid num workers"),
            Self::InvalidWorkerIndex => write!(f, "invalid worker index"),
            Self::HeaderMismatch => write!(f, "header mismatch"),
            Self::NoAvailableWorkers => write!(f, "no available workers"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_version_display() {
        let expected: u32 = 1u32 << 16; // 1.0
        let actual: u32 = (3u32 << 16) | 7; // 3.7
        let err = Error::InvalidVersion { expected, actual };
        assert_eq!(err.to_string(), "invalid version; expected=1.0; found=3.7");
    }
}
