use super::Error;

#[derive(Clone, Copy, Debug)]
pub(super) enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn u16(self, b: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(b),
            Self::Big => u16::from_be_bytes(b),
        }
    }
    pub fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(b),
            Self::Big => u32::from_be_bytes(b),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Entry {
    pub tag: u16,
    pub kind: u16,
    pub count: u32,
    pub value: [u8; 4],
}

impl Entry {
    pub fn parse(b: &[u8], endian: Endian) -> Result<Self, Error> {
        let b: &[u8; 12] = b
            .try_into()
            .map_err(|_| Error::Invalid("short IFD entry"))?;
        Ok(Self {
            tag: endian.u16([b[0], b[1]]),
            kind: endian.u16([b[2], b[3]]),
            count: endian.u32([b[4], b[5], b[6], b[7]]),
            value: [b[8], b[9], b[10], b[11]],
        })
    }

    pub fn scalar(self, endian: Endian) -> Option<u64> {
        if self.count != 1 {
            return None;
        }
        match self.kind {
            1 => Some(self.value[0] as u64),
            3 => Some(endian.u16([self.value[0], self.value[1]]) as u64),
            4 | 13 => Some(endian.u32(self.value) as u64),
            _ => None,
        }
    }

    pub fn byte_len(self) -> Option<u64> {
        let size = match self.kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        u64::from(self.count).checked_mul(size)
    }
}

pub(super) fn checked_range(offset: u64, length: u64, file_len: u64) -> Result<(), Error> {
    if offset
        .checked_add(length)
        .filter(|end| *end <= file_len)
        .is_none()
    {
        return Err(Error::Invalid(
            "byte range outside file or integer overflow",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endian_and_inline_short() {
        assert_eq!(Endian::Little.u32([0x78, 0x56, 0x34, 0x12]), 0x12345678);
        assert_eq!(Endian::Big.u32([0x12, 0x34, 0x56, 0x78]), 0x12345678);
        let entry = Entry {
            tag: 0,
            kind: 3,
            count: 1,
            value: [0x12, 0x34, 0, 0],
        };
        assert_eq!(entry.scalar(Endian::Big), Some(0x1234));
        assert_eq!(entry.scalar(Endian::Little), Some(0x3412));
    }

    #[test]
    fn rejects_overflows_and_short_entries() {
        assert!(checked_range(u64::MAX, 1, u64::MAX).is_err());
        assert!(checked_range(3, 8, 10).is_err());
        assert!(checked_range(10, 0, 10).is_ok());
        assert!(Entry::parse(&[0; 11], Endian::Little).is_err());
    }
}
