use crate::fmt::debug;
use embassy_time::{Duration, Instant};
use embedded_hal_async::i2c::Error;

use crate::error::PedometerResult;

const ADDRESS: u8 = 0b1101010;
const NUM_REGS: u8 = 0x76;

#[derive(Debug, Copy, Clone, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Steps {
    pub steps: u16,
    pub timestamp: Timestamp,
}

impl Steps {
    fn from_step_registers(buf: [u8; 4]) -> Self {
        Self {
            steps: u16::from_le_bytes(buf[2..4].try_into().unwrap()),
            timestamp: Timestamp::from_step_registers(buf),
        }
    }

    fn from_fifo(buf: [u8; 6]) -> Self {
        Self {
            steps: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
            timestamp: Timestamp::from_fifo(buf),
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Timestamp(u32);

impl Timestamp {
    fn from_step_registers(buf: [u8; 4]) -> Self {
        Self((u16::from_le_bytes(buf[0..2].try_into().unwrap()) as u32) << 8)
    }

    fn from_time_registers(buf: [u8; 3]) -> Self {
        Self(u16::from_le_bytes(buf[..2].try_into().unwrap()) as u32 | ((buf[2] as u32) << 16))
    }

    fn from_fifo(buf: [u8; 6]) -> Self {
        Self(((u16::from_le_bytes(buf[0..2].try_into().unwrap()) as u32) << 8) | buf[3] as u32)
    }

    pub fn as_duration(self) -> Duration {
        Duration::from_micros(self.0 as u64 * 6400)
    }

    /// It is always assumed that self is before imu_now and there was at most one timer overflow
    /// between.
    pub fn to_instant(self, mcu_now: Instant, imu_now: Self) -> Instant {
        let imu_time_diff = Self(imu_now.0.overflowing_sub(self.0).0);
        mcu_now - imu_time_diff.as_duration()
    }
}

#[repr(u8)]
enum Register {
    FifoCtrl1 = 0x06,
    FifoCtrl2 = 0x07,
    FifoCtrl4 = 0x09,
    FifoCtrl5 = 0x0A,
    Int1Ctrl = 0x0D,
    Int2Ctrl = 0x0E,
    Ctrl1Xl = 0x10,
    Ctrl3C = 0x12,
    Ctrl10C = 0x19,
    FifoStatus1 = 0x3A,
    FifoDataOutL = 0x3E,
    Timestamp0Reg = 0x40,
    StepTimestampL = 0x49,
}

pub(crate) struct Imu<I: embedded_hal_async::i2c::I2c> {
    i2c: I,
}

impl<I: embedded_hal_async::i2c::I2c> Imu<I> {
    pub fn new(i2c: I) -> Self {
        Self { i2c }
    }

    pub async fn init(&mut self) -> PedometerResult<()> {
        // Enable Block Data Update
        self.write_register(Register::Ctrl3C as u8, 0x44).await?;
        Ok(())
    }

    pub async fn read_register(&mut self, register_addr: u8) -> PedometerResult<u8> {
        let mut buf = [0; 1];
        self.read_register_range(register_addr, &mut buf).await?;
        Ok(buf[0])
    }

    pub async fn read_register_range(
        &mut self,
        start_addr: u8,
        buf: &mut [u8],
    ) -> PedometerResult<()> {
        self.i2c
            .write_read(ADDRESS, &[start_addr], buf)
            .await
            .map_err(|e| e.kind())?;
        Ok(())
    }

    pub async fn read_all_registers(&mut self) -> PedometerResult<[u8; NUM_REGS as usize]> {
        let mut buf = [0; NUM_REGS as usize];
        self.read_register_range(0, &mut buf).await?;
        Ok(buf)
    }

    pub async fn dump_all_registers(&mut self) -> PedometerResult<()> {
        let buf = self.read_all_registers().await?;
        debug!("IMU registers:");
        for (i, b) in buf.iter().enumerate() {
            debug!("0x{0:02x}: 0x{1:02x} (0b{1:08b})", i, *b);
        }
        Ok(())
    }

    pub async fn write_register(&mut self, register_addr: u8, value: u8) -> PedometerResult<()> {
        self.i2c
            .write(ADDRESS, &[register_addr, value])
            .await
            .map_err(|e| e.kind())?;
        Ok(())
    }

    pub async fn enable_pedometer(&mut self, enable_interrupt: bool) -> PedometerResult<()> {
        // 1. Write 20h to CTRL1_XL // Turn on the accelerometer: ODR_XL = 26 Hz, FS_XL = ±2 g
        self.write_register(Register::Ctrl1Xl as u8, 0x20).await?;
        // 2. Write 34h to CTRL10_C // Enable embedded functions, pedometer algorithm and timestamp
        self.write_register(Register::Ctrl10C as u8, 0x34).await?;
        if enable_interrupt {
            // 3. Write 80h to INT1_CTRL // Step detector interrupt driven to INT1 pin
            self.write_register(Register::Int1Ctrl as u8, 0x80).await?;
        }
        // 4. Write 40h to INT2_CTRL // Enable step count overflow interrupt which is not
        //    connected but resets the counter automatically on overflow
        self.write_register(Register::Int2Ctrl as u8, 0x40).await?;
        Ok(())
    }

    pub async fn enable_fifo_for_pedometer(
        &mut self,
        interrupt_threshold: Option<u16>,
    ) -> PedometerResult<()> {
        if let Some(interrupt_threshold) = interrupt_threshold {
            if interrupt_threshold >= 2_u16.pow(11) {
                return Err(crate::error::PedometerFwError::Misc);
            }
        }

        // Choose the decimation factor for the 4th FIFO data set through the DEC_DS4_FIFO[2:0] bits of the FIFO_CTRL4 register => 0b101 (8)
        self.write_register(Register::FifoCtrl4 as u8, 0x28).await?;

        // Set to 1 the TIMER_PEDO_FIFO_EN bit in the FIFO_CTRL2 register
        // Configure the bit TIMER_PEDO_FIFO_DRDY in the FIFO_CTRL2 register in order to choose the method of storing data in the FIFO (internal trigger or every step detected)
        let mut fifo_ctrl2 = 0xC0;
        if let Some(interrupt_threshold) = interrupt_threshold {
            // set threshold registers/values
            fifo_ctrl2 |= (interrupt_threshold >> 8) as u8;
            self.write_register(
                Register::FifoCtrl1 as u8,
                (interrupt_threshold & 0xFF) as u8,
            )
            .await?;
            // Enable threshold interrupt
            self.write_register(Register::Int1Ctrl as u8, 0x08).await?;
        }
        self.write_register(Register::FifoCtrl2 as u8, fifo_ctrl2)
            .await?;

        // Configure the FIFO operating mode through the FIFO_MODE_[2:0] field of the FIFO_CTRL5 register.
        //
        // IMPORTANT: Apparently this is not enough and ODR_FIFO_[3:0] has to be set as well contrary to what is
        // written in AN5130
        self.write_register(Register::FifoCtrl5 as u8, 0b10110)
            .await?;
        Ok(())
    }

    pub async fn read_steps_from_registers(&mut self) -> PedometerResult<Steps> {
        let mut buf = [0; 4];
        self.read_register_range(Register::StepTimestampL as u8, &mut buf)
            .await?;
        Ok(Steps::from_step_registers(buf))
    }

    pub async fn read_steps_from_fifo(&mut self) -> PedometerResult<Option<Steps>> {
        let unread_words = self.read_register(Register::FifoStatus1 as u8).await?;
        debug!("Unread fifo words: {}", unread_words);
        if unread_words < 3 {
            return Ok(None);
        }

        let mut buf = [0; 6];
        for i in 0..3 {
            self.read_register_range(Register::FifoDataOutL as u8, &mut buf[i * 2..i * 2 + 2])
                .await?;
        }
        debug!("Step buf: {:?}", buf);
        Ok(Some(Steps::from_fifo(buf)))
    }

    pub async fn read_timestamp(&mut self) -> PedometerResult<Timestamp> {
        let mut buf = [0; 3];
        self.read_register_range(Register::Timestamp0Reg as u8, &mut buf)
            .await?;
        debug!("Timestamp registers: {:?}", buf);
        Ok(Timestamp::from_time_registers(buf))
    }
}
