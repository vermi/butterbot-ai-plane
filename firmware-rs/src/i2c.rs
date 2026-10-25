//! I2C on ESP-IDF's master driver. The driver locks the bus per transfer, so devices can be used
//! from any thread. The camera's SCCB shares the main bus (esp32-camera finds it by port).

use esp_idf_svc::sys::{self, esp, EspError};

const TIMEOUT_MS: i32 = 50;

pub struct Bus(sys::i2c_master_bus_handle_t);

unsafe impl Send for Bus {}
unsafe impl Sync for Bus {}

impl Bus {
    pub fn new(port: i32, sda: i32, scl: i32) -> Result<Bus, EspError> {
        let mut cfg: sys::i2c_master_bus_config_t = unsafe { core::mem::zeroed() };
        cfg.i2c_port = port;
        cfg.sda_io_num = sda;
        cfg.scl_io_num = scl;
        cfg.__bindgen_anon_1.clk_source = sys::soc_periph_i2c_clk_src_t_I2C_CLK_SRC_DEFAULT;
        cfg.glitch_ignore_cnt = 7;
        let mut handle = core::ptr::null_mut();
        esp!(unsafe { sys::i2c_new_master_bus(&cfg, &mut handle) })?;
        Ok(Bus(handle))
    }

    pub fn probe(&self, addr: u16) -> bool {
        unsafe { sys::i2c_master_probe(self.0, addr, TIMEOUT_MS) == sys::ESP_OK }
    }

    pub fn device(&self, addr: u16, hz: u32) -> Result<Dev, EspError> {
        let mut cfg: sys::i2c_device_config_t = unsafe { core::mem::zeroed() };
        cfg.dev_addr_length = sys::i2c_addr_bit_len_t_I2C_ADDR_BIT_LEN_7;
        cfg.device_address = addr;
        cfg.scl_speed_hz = hz;
        let mut handle = core::ptr::null_mut();
        esp!(unsafe { sys::i2c_master_bus_add_device(self.0, &cfg, &mut handle) })?;
        Ok(Dev(handle))
    }
}

pub struct Dev(sys::i2c_master_dev_handle_t);

unsafe impl Send for Dev {}
unsafe impl Sync for Dev {}

impl Dev {
    pub fn write(&self, data: &[u8]) -> Result<(), EspError> {
        esp!(unsafe { sys::i2c_master_transmit(self.0, data.as_ptr(), data.len(), TIMEOUT_MS) })
    }

    pub fn write_read(&self, w: &[u8], r: &mut [u8]) -> Result<(), EspError> {
        esp!(unsafe {
            sys::i2c_master_transmit_receive(
                self.0,
                w.as_ptr(),
                w.len(),
                r.as_mut_ptr(),
                r.len(),
                TIMEOUT_MS,
            )
        })
    }

    pub fn read(&self, out: &mut [u8]) -> Result<(), EspError> {
        esp!(unsafe { sys::i2c_master_receive(self.0, out.as_mut_ptr(), out.len(), TIMEOUT_MS) })
    }

    /// Register write: `reg` followed by `data` (at most 15 bytes).
    pub fn write_reg(&self, reg: u8, data: &[u8]) -> Result<(), EspError> {
        let mut buf = [0u8; 16];
        let n = data.len().min(15);
        buf[0] = reg;
        buf[1..=n].copy_from_slice(&data[..n]);
        self.write(&buf[..=n])
    }

    pub fn read_reg(&self, reg: u8, out: &mut [u8]) -> Result<(), EspError> {
        self.write_read(&[reg], out)
    }

    pub fn read_u8(&self, reg: u8) -> Result<u8, EspError> {
        let mut b = [0u8];
        self.read_reg(reg, &mut b)?;
        Ok(b[0])
    }
}
