//! Reading and writing ASIO's many sample formats as f32.

pub(crate) unsafe fn write_asio_sample(
    ptr: *mut u8,
    frame: usize,
    sample_type: azo::sys::SampleType,
    value: f32,
) {
    use azo::sys::SampleType;
    use std::ptr;

    let value = value.clamp(-1.0, 1.0);
    let sample_size = match sample_type {
        SampleType::PCM_I16_MSB | SampleType::PCM_I16_LSB => 2,
        SampleType::PCM_I24_MSB | SampleType::PCM_I24_LSB => 3,
        SampleType::PCM_I32_MSB
        | SampleType::PCM_I32_LSB
        | SampleType::PCM_F32_MSB
        | SampleType::PCM_F32_LSB
        | SampleType::PCM_I32_MSB_16
        | SampleType::PCM_I32_LSB_16
        | SampleType::PCM_I32_MSB_18
        | SampleType::PCM_I32_LSB_18
        | SampleType::PCM_I32_MSB_20
        | SampleType::PCM_I32_LSB_20
        | SampleType::PCM_I32_MSB_24
        | SampleType::PCM_I32_LSB_24 => 4,
        SampleType::PCM_F64_MSB | SampleType::PCM_F64_LSB => 8,
        _ => return,
    };
    let p = unsafe { ptr.add(frame * sample_size) };
    let i16_value = (value * 32767.0).round() as i16;
    let i24_value = (value * 8_388_607.0).round() as i32;
    let i32_value = (value * 2_147_483_647.0).round() as i32;
    match sample_type {
        SampleType::PCM_F32_MSB => unsafe {
            ptr::write_unaligned(p.cast::<u32>(), value.to_bits().to_be());
        },
        SampleType::PCM_F32_LSB => unsafe {
            ptr::write_unaligned(p.cast::<f32>(), value);
        },
        SampleType::PCM_F64_MSB => unsafe {
            ptr::write_unaligned(p.cast::<u64>(), (value as f64).to_bits().to_be());
        },
        SampleType::PCM_F64_LSB => unsafe {
            ptr::write_unaligned(p.cast::<f64>(), value as f64);
        },
        SampleType::PCM_I16_MSB => unsafe {
            ptr::write_unaligned(p.cast::<i16>(), i16_value.to_be());
        },
        SampleType::PCM_I16_LSB => unsafe {
            ptr::write_unaligned(p.cast::<i16>(), i16_value);
        },
        SampleType::PCM_I24_MSB => {
            let bytes = [
                ((i24_value >> 16) & 0xff) as u8,
                ((i24_value >> 8) & 0xff) as u8,
                (i24_value & 0xff) as u8,
            ];
            unsafe { ptr::write_unaligned(p.cast::<[u8; 3]>(), bytes) };
        }
        SampleType::PCM_I24_LSB => {
            let bytes = [
                (i24_value & 0xff) as u8,
                ((i24_value >> 8) & 0xff) as u8,
                ((i24_value >> 16) & 0xff) as u8,
            ];
            unsafe { ptr::write_unaligned(p.cast::<[u8; 3]>(), bytes) };
        }
        SampleType::PCM_I32_MSB => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), i32_value.to_be());
        },
        SampleType::PCM_I32_LSB => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), i32_value);
        },
        SampleType::PCM_I32_MSB_16 => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), (i16_value as i32).wrapping_shl(16).to_be());
        },
        SampleType::PCM_I32_LSB_16 => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), (i16_value as i32).wrapping_shl(16));
        },
        SampleType::PCM_I32_MSB_18 => {
            let v = ((value * 131_071.0).round() as i32).wrapping_shl(14);
            unsafe { ptr::write_unaligned(p.cast::<i32>(), v.to_be()) };
        }
        SampleType::PCM_I32_LSB_18 => {
            let v = ((value * 131_071.0).round() as i32).wrapping_shl(14);
            unsafe { ptr::write_unaligned(p.cast::<i32>(), v) };
        }
        SampleType::PCM_I32_MSB_20 => {
            let v = ((value * 524_287.0).round() as i32).wrapping_shl(12);
            unsafe { ptr::write_unaligned(p.cast::<i32>(), v.to_be()) };
        }
        SampleType::PCM_I32_LSB_20 => {
            let v = ((value * 524_287.0).round() as i32).wrapping_shl(12);
            unsafe { ptr::write_unaligned(p.cast::<i32>(), v) };
        }
        SampleType::PCM_I32_MSB_24 => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), i24_value.wrapping_shl(8).to_be());
        },
        SampleType::PCM_I32_LSB_24 => unsafe {
            ptr::write_unaligned(p.cast::<i32>(), i24_value.wrapping_shl(8));
        },
        _ => {}
    }
}

pub(crate) unsafe fn read_asio_sample(
    ptr: *const u8,
    frame: usize,
    sample_type: azo::sys::SampleType,
) -> f32 {
    use azo::sys::SampleType;
    use std::ptr;

    let sample_size = match sample_type {
        SampleType::PCM_I16_MSB | SampleType::PCM_I16_LSB => 2,
        SampleType::PCM_I24_MSB | SampleType::PCM_I24_LSB => 3,
        SampleType::PCM_I32_MSB
        | SampleType::PCM_I32_LSB
        | SampleType::PCM_F32_MSB
        | SampleType::PCM_F32_LSB
        | SampleType::PCM_I32_MSB_16
        | SampleType::PCM_I32_LSB_16
        | SampleType::PCM_I32_MSB_18
        | SampleType::PCM_I32_LSB_18
        | SampleType::PCM_I32_MSB_20
        | SampleType::PCM_I32_LSB_20
        | SampleType::PCM_I32_MSB_24
        | SampleType::PCM_I32_LSB_24 => 4,
        SampleType::PCM_F64_MSB | SampleType::PCM_F64_LSB => 8,
        _ => return 0.0,
    };
    let p = unsafe { ptr.add(frame * sample_size) };
    let value = match sample_type {
        SampleType::PCM_F32_MSB => f32::from_bits(u32::from_be(unsafe {
            ptr::read_unaligned(p.cast::<u32>())
        })),
        SampleType::PCM_F32_LSB => unsafe { ptr::read_unaligned(p.cast::<f32>()) },
        SampleType::PCM_F64_MSB => f64::from_bits(u64::from_be(unsafe {
            ptr::read_unaligned(p.cast::<u64>())
        })) as f32,
        SampleType::PCM_F64_LSB => unsafe { ptr::read_unaligned(p.cast::<f64>()) as f32 },
        SampleType::PCM_I16_MSB => {
            i16::from_be(unsafe { ptr::read_unaligned(p.cast::<i16>()) }) as f32 / 32768.0
        }
        SampleType::PCM_I16_LSB => {
            (unsafe { ptr::read_unaligned(p.cast::<i16>()) }) as f32 / 32768.0
        }
        SampleType::PCM_I24_MSB => {
            let raw = unsafe { ptr::read_unaligned(p.cast::<[u8; 3]>()) };
            let value = ((raw[0] as i32) << 24 | (raw[1] as i32) << 16 | (raw[2] as i32) << 8) >> 8;
            value as f32 / 8_388_608.0
        }
        SampleType::PCM_I24_LSB => {
            let raw = unsafe { ptr::read_unaligned(p.cast::<[u8; 3]>()) };
            let value = ((raw[2] as i32) << 24 | (raw[1] as i32) << 16 | (raw[0] as i32) << 8) >> 8;
            value as f32 / 8_388_608.0
        }
        SampleType::PCM_I32_MSB => {
            i32::from_be(unsafe { ptr::read_unaligned(p.cast::<i32>()) }) as f32 / 2_147_483_648.0
        }
        SampleType::PCM_I32_LSB => {
            (unsafe { ptr::read_unaligned(p.cast::<i32>()) }) as f32 / 2_147_483_648.0
        }
        SampleType::PCM_I32_MSB_16 => {
            (i32::from_be(unsafe { ptr::read_unaligned(p.cast::<i32>()) }) >> 16) as f32 / 32768.0
        }
        SampleType::PCM_I32_LSB_16 => {
            (unsafe { ptr::read_unaligned(p.cast::<i32>()) } >> 16) as f32 / 32768.0
        }
        SampleType::PCM_I32_MSB_18 => {
            (i32::from_be(unsafe { ptr::read_unaligned(p.cast::<i32>()) }) >> 14) as f32 / 131_072.0
        }
        SampleType::PCM_I32_LSB_18 => {
            (unsafe { ptr::read_unaligned(p.cast::<i32>()) } >> 14) as f32 / 131_072.0
        }
        SampleType::PCM_I32_MSB_20 => {
            (i32::from_be(unsafe { ptr::read_unaligned(p.cast::<i32>()) }) >> 12) as f32 / 524_288.0
        }
        SampleType::PCM_I32_LSB_20 => {
            (unsafe { ptr::read_unaligned(p.cast::<i32>()) } >> 12) as f32 / 524_288.0
        }
        SampleType::PCM_I32_MSB_24 => {
            (i32::from_be(unsafe { ptr::read_unaligned(p.cast::<i32>()) }) >> 8) as f32
                / 8_388_608.0
        }
        SampleType::PCM_I32_LSB_24 => {
            (unsafe { ptr::read_unaligned(p.cast::<i32>()) } >> 8) as f32 / 8_388_608.0
        }
        _ => 0.0,
    };
    value.clamp(-1.0, 1.0)
}
