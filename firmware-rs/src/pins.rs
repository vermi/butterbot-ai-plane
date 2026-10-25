//! The ButterBot board's wiring.
#![allow(dead_code)]

/// Driven low, it switches the robot off. Left as an input, the power stays on.
pub const PWDN: i32 = 4;
/// The power button: high while pressed.
pub const BUTTON: i32 = 42;
/// Battery voltage divider (ADC1).
pub const BATTERY: i32 = 6;
/// The base board's interrupt line: high while it has events.
pub const BASE_INT: i32 = 20;
pub const ACCEL_INT: i32 = 19;

pub const I2C_MAIN_SDA: i32 = 47;
pub const I2C_MAIN_SCL: i32 = 48;
/// The add-on card slot's own bus.
pub const I2C_CARD_SDA: i32 = 41;
pub const I2C_CARD_SCL: i32 = 40;

pub const CAM_PWDN: i32 = 18;
pub const CAM_XCLK: i32 = 15;
pub const CAM_D7: i32 = 16;
pub const CAM_D6: i32 = 14;
pub const CAM_D5: i32 = 13;
pub const CAM_D4: i32 = 11;
pub const CAM_D3: i32 = 9;
pub const CAM_D2: i32 = 7;
pub const CAM_D1: i32 = 8;
pub const CAM_D0: i32 = 10;
pub const CAM_VSYNC: i32 = 21;
pub const CAM_HREF: i32 = 17;
pub const CAM_PCLK: i32 = 12;

/// Speaker (I2S1, standard MSB).
pub const SPK_BCK: i32 = 45;
pub const SPK_WS: i32 = 5;
pub const SPK_DOUT: i32 = 3;
/// Microphone (I2S0, PDM).
pub const MIC_CLK: i32 = 46;
pub const MIC_DIN: i32 = 1;

/// Card slot control lines on the chip.
pub const CTRL_1: i32 = 39;
pub const CTRL_2: i32 = 38;
pub const CTRL_6: i32 = 2;

/// I2C addresses on the main bus.
pub const ADDR_TCA: u16 = 0x20;
pub const ADDR_BASE: u16 = 0x69;
pub const ADDR_ACCEL: u16 = 0x18;
pub const ADDR_RTC: u16 = 0x51;

/// TCA9555 expander pins.
pub mod exp {
    pub const LED: u8 = 0;
    pub const CALIB_EN: u8 = 1;
    pub const SPK_EN: u8 = 4;
    pub const DET_2: u8 = 5;
    pub const CTRL_3: u8 = 6;
    pub const CTRL_5: u8 = 7;
    pub const CTRL_4: u8 = 8;
    pub const DET_1: u8 = 9;
    pub const ADDR_1: u8 = 10;
    pub const ADDR_2: u8 = 11;
    pub const ADDR_3: u8 = 12;
    pub const ADDR_4: u8 = 13;
    pub const ADDR_5: u8 = 14;
    pub const ADDR_6: u8 = 15;
}
