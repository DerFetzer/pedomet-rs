pub(crate) struct Imu<I: embedded_hal_async::i2c::I2c> {
    i2c: I,
}
