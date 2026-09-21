use std::io::{self, Read, Seek, SeekFrom};

const HEADER_BYTES: usize = 64;
const PROGRAM_HEADER_BYTES: u16 = 56;
const MAX_PROGRAM_HEADERS: u16 = 512;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_INTERP: u32 = 3;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "image is not an ELF64 x86-64 executable",
    )
}

pub fn validate_elf64_x86_64(image: &mut (impl Read + Seek)) -> io::Result<()> {
    let length = image.seek(SeekFrom::End(0))?;
    let mut header = [0_u8; HEADER_BYTES];
    image.seek(SeekFrom::Start(0))?;
    image.read_exact(&mut header).map_err(|_| invalid())?;
    let u16_at = |offset: usize| u16::from_le_bytes([header[offset], header[offset + 1]]);
    let u32_at = |offset: usize| {
        u32::from_le_bytes(header[offset..offset + 4].try_into().expect("fixed range"))
    };
    let u64_at = |offset: usize| {
        u64::from_le_bytes(header[offset..offset + 8].try_into().expect("fixed range"))
    };
    let kind = u16_at(16);
    let program_header_offset = u64_at(32);
    let program_header_count = u16_at(56);
    if header[0..4] != *b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || !matches!(kind, ET_EXEC | ET_DYN)
        || u16_at(18) != EM_X86_64
        || u32_at(20) != 1
        || usize::from(u16_at(52)) != HEADER_BYTES
        || u16_at(54) != PROGRAM_HEADER_BYTES
        || program_header_count == 0
        || program_header_count > MAX_PROGRAM_HEADERS
    {
        return Err(invalid());
    }
    let table_bytes = u64::from(program_header_count) * u64::from(PROGRAM_HEADER_BYTES);
    let table_end = program_header_offset
        .checked_add(table_bytes)
        .ok_or_else(invalid)?;
    if program_header_offset < HEADER_BYTES as u64 || table_end > length {
        return Err(invalid());
    }
    let mut table = vec![0_u8; usize::try_from(table_bytes).map_err(|_| invalid())?];
    image.seek(SeekFrom::Start(program_header_offset))?;
    image.read_exact(&mut table).map_err(|_| invalid())?;
    let has_interpreter = table
        .chunks_exact(usize::from(PROGRAM_HEADER_BYTES))
        .any(|entry| u32::from_le_bytes(entry[0..4].try_into().expect("fixed range")) == PT_INTERP);
    if kind == ET_DYN && !has_interpreter {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub struct TestElf {
        pub class: u8,
        pub data: u8,
        pub kind: u16,
        pub machine: u16,
        pub interpreter: bool,
        pub program_headers: u16,
    }

    impl TestElf {
        pub const fn executable() -> Self {
            Self {
                class: 2,
                data: 1,
                kind: 2,
                machine: 62,
                interpreter: false,
                program_headers: 1,
            }
        }

        pub const fn position_independent() -> Self {
            Self {
                kind: 3,
                interpreter: true,
                ..Self::executable()
            }
        }

        pub fn bytes(&self) -> Vec<u8> {
            let count = usize::from(self.program_headers);
            let mut image = vec![0_u8; 64 + count.max(1) * 56];
            image[0..4].copy_from_slice(b"\x7fELF");
            image[4] = self.class;
            image[5] = self.data;
            image[6] = 1;
            image[16..18].copy_from_slice(&self.kind.to_le_bytes());
            image[18..20].copy_from_slice(&self.machine.to_le_bytes());
            image[20..24].copy_from_slice(&1_u32.to_le_bytes());
            image[32..40].copy_from_slice(&64_u64.to_le_bytes());
            image[52..54].copy_from_slice(&64_u16.to_le_bytes());
            image[54..56].copy_from_slice(&56_u16.to_le_bytes());
            image[56..58].copy_from_slice(&self.program_headers.to_le_bytes());
            for index in 0..count {
                let offset = 64 + index * 56;
                let kind: u32 = if self.interpreter && index == 0 { 3 } else { 1 };
                image[offset..offset + 4].copy_from_slice(&kind.to_le_bytes());
            }
            image
        }

        #[cfg(unix)]
        pub fn write(&self, path: &std::path::Path) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(path, self.bytes()).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::TestElf, validate_elf64_x86_64};
    use std::io::Cursor;

    fn valid(bytes: &[u8]) -> bool {
        validate_elf64_x86_64(&mut Cursor::new(bytes)).is_ok()
    }

    #[test]
    fn accepts_static_executables_and_pie_with_an_interpreter() {
        assert!(valid(&TestElf::executable().bytes()));
        assert!(valid(&TestElf::position_independent().bytes()));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let current = std::env::current_exe().unwrap();
            validate_elf64_x86_64(&mut std::fs::File::open(current).unwrap())
                .expect("the native test binary must remain accepted");
        }
    }

    #[test]
    fn rejects_foreign_incoherent_and_library_images() {
        for invalid in [
            TestElf {
                class: 1,
                ..TestElf::executable()
            },
            TestElf {
                data: 2,
                ..TestElf::executable()
            },
            TestElf {
                machine: 183,
                ..TestElf::executable()
            },
            TestElf {
                kind: 1,
                ..TestElf::executable()
            },
            TestElf {
                kind: 3,
                interpreter: false,
                ..TestElf::executable()
            },
            TestElf {
                program_headers: 0,
                ..TestElf::executable()
            },
            TestElf {
                program_headers: 513,
                ..TestElf::executable()
            },
        ] {
            assert!(!valid(&invalid.bytes()));
        }
        let complete = TestElf::executable().bytes();
        assert!(!valid(&complete[..63]));
        assert!(!valid(&complete[..100]));
        let mut out_of_range = complete.clone();
        out_of_range[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(!valid(&out_of_range));
        let mut wrong_entry_size = complete;
        wrong_entry_size[54..56].copy_from_slice(&32_u16.to_le_bytes());
        assert!(!valid(&wrong_entry_size));
    }
}
