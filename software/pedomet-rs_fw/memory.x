MEMORY
{
  /* NOTE 1 K = 1 KiBi = 1024 bytes */
  /* You must fill in these values for your application */
  FLASH : ORIGIN = 0x00000000 + 156K, LENGTH = 1024K - 156K - 64K
  RAM : ORIGIN = 0x20000000 + 12K, LENGTH = 256K - 12K
}
