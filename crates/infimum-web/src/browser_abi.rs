//! Generic `.ifw` Browser Host ABI constants & enums.
//!
//! Shared across all Infimum capsules (zero application-specific definitions).

pub const GPU_CMD_BYTE_OFFSET: i64 = 262144;
pub const GPU_CLIPS_BYTE_OFFSET: i64 = 256;
pub const SYS_INPUT_BYTE_OFFSET: i64 = 768;
pub const SYS_FONT_METRICS_BYTE_OFFSET: i64 = 1024;
pub const CLIPBOARD_MAX_CHARS: i64 = 4096;
pub const INPUT_MAX_CHARS: i64 = 8192;
pub const LARGE_COPY_MAX_CHARS: i64 = 32768;

pub const PASTE_MAILBOX_WORD: usize = 32000;
#[allow(dead_code)]
pub const FONT_METRICS_FLAG_WORD: usize = 256;
#[allow(dead_code)]
pub const FONT_METRICS_TABLE_WORD: usize = 257;

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorKind {
    Default = 0,
    Pointer = 1,
    Text = 2,
    EwResize = 3,
}

impl core::convert::TryFrom<i32> for CursorKind {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Default),
            1 => Ok(Self::Pointer),
            2 => Ok(Self::Text),
            3 => Ok(Self::EwResize),
            other => Err(other),
        }
    }
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAction {
    None = 0,
    Copy = 1,
    Paste = 2,
    Cut = 3,
    PickImage = 4,
    ClearImage = 5,
    RemoveImage0 = 6,
    RemoveImage1 = 7,
    RemoveImage2 = 8,
    RemoveImage3 = 9,
    RemoveImage4 = 10,
    RemoveImage5 = 11,
}

impl core::convert::TryFrom<i32> for HostAction {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Copy),
            2 => Ok(Self::Paste),
            3 => Ok(Self::Cut),
            4 => Ok(Self::PickImage),
            5 => Ok(Self::ClearImage),
            6 => Ok(Self::RemoveImage0),
            7 => Ok(Self::RemoveImage1),
            8 => Ok(Self::RemoveImage2),
            9 => Ok(Self::RemoveImage3),
            10 => Ok(Self::RemoveImage4),
            11 => Ok(Self::RemoveImage5),
            other => Err(other),
        }
    }
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeySpecial {
    None = 0,
    Backspace = 1,
    Enter = 2,
    Escape = 3,
    Tab = 4,
    ArrowLeft = 5,
    ArrowRight = 6,
    ArrowUp = 7,
    ArrowDown = 8,
    Home = 9,
    End = 10,
    Delete = 11,
    ShiftEnter = 12,
    CtrlA = 20,
    CtrlC = 21,
    CtrlV = 22,
    CtrlX = 23,
    ShiftLeft = 25,
    ShiftRight = 26,
    ShiftUp = 27,
    ShiftDown = 28,
    ShiftHome = 29,
    ShiftEnd = 30,
    RightClick = 31,
    CtrlHome = 32,
    CtrlEnd = 33,
    CtrlShiftHome = 34,
    CtrlShiftEnd = 35,
}
