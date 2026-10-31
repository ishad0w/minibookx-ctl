use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    path::Path,
};

pub const MANUAL: u8 = 0x40;
pub const MAX_RAW: u8 = 74;
pub const MIN_RAW: u8 = 32;
pub const PWM_PERIOD: u8 = 91;
pub const PWM_COUNTS: u16 = PWM_PERIOD as u16 + 1;

pub fn raw_percent(raw: u8) -> f64 {
    f64::from(raw) / f64::from(PWM_COUNTS) * 100.
}

pub trait EcIo {
    fn read(&self, register: u8) -> io::Result<u8>;
    fn write(&mut self, register: u8, value: u8) -> io::Result<()>;
}

pub struct FileEc(File);
impl FileEc {
    pub fn open(path: &Path, writable: bool) -> io::Result<Self> {
        Ok(Self(
            OpenOptions::new().read(true).write(writable).open(path)?,
        ))
    }
}
impl EcIo for FileEc {
    fn read(&self, register: u8) -> io::Result<u8> {
        if ![0x4a, 0x4b, 0x4f, 0x70, 0x76, 0x77].contains(&register) {
            return Err(io::Error::other("EC read outside whitelist"));
        }
        let mut b = [0];
        if self.0.read_at(&mut b, u64::from(register))? != 1 {
            return Err(io::Error::other("short EC read"));
        }
        Ok(b[0])
    }
    fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
        match register {
            0x4b if value == 0 || (MIN_RAW..=MAX_RAW).contains(&value) => {
                if self.read(0x4f)? & MANUAL == 0 {
                    return Err(io::Error::other("duty write while auto"));
                }
            }
            0x4f => {
                if (self.read(0x4f)? ^ value) & !MANUAL != 0 {
                    return Err(io::Error::other("unrelated EC flag change"));
                }
            }
            _ => return Err(io::Error::other("EC write outside bounded whitelist")),
        }
        if self.0.write_at(&[value], u64::from(register))? != 1 {
            return Err(io::Error::other(
                "short EC write, result may be indeterminate",
            ));
        }
        Ok(())
    }
}

pub struct Fan<T: EcIo> {
    io: T,
    may_be_manual: bool,
    current: Option<u8>,
}
impl<T: EcIo> Fan<T> {
    pub fn new(io: T) -> Self {
        Self {
            io,
            may_be_manual: false,
            current: None,
        }
    }
    pub fn baseline(&self) -> io::Result<()> {
        if self.io.read(0x4a)? != PWM_PERIOD {
            return Err(io::Error::other("unsupported PWM period"));
        }
        if self.io.read(0x4f)? & MANUAL != 0 {
            return Err(io::Error::other("manual already owned"));
        }
        if self.io.read(0x4b)? > MAX_RAW {
            return Err(io::Error::other("unexpected stock duty"));
        }
        Ok(())
    }
    /// Caller must have persisted ownership BEFORE this potentially side-effecting call.
    pub fn activate(&mut self) -> io::Result<()> {
        self.baseline()?;
        let flags = self.io.read(0x4f)?;
        if flags & MANUAL != 0 {
            return Err(io::Error::other("manual acquired by another controller"));
        }
        self.may_be_manual = true;
        self.io.write(0x4f, flags | MANUAL)?;
        if self.io.read(0x4f)? & MANUAL == 0 {
            return Err(io::Error::other("manual flag did not latch"));
        }
        // Read again after latching manual mode: firmware may have raised its
        // duty between baseline and takeover. Never acknowledge an assumed 0.
        let current = self.io.read(0x4b)?;
        if current != 0 && !(MIN_RAW..=MAX_RAW).contains(&current) {
            return Err(io::Error::other(
                "unsupported takeover duty; recovery required",
            ));
        }
        self.current = Some(current);
        Ok(())
    }
    pub fn apply(&mut self, raw: u8) -> io::Result<bool> {
        if raw != 0 && !(MIN_RAW..=MAX_RAW).contains(&raw) {
            return Err(io::Error::other("unvalidated raw PWM"));
        }
        if !self.may_be_manual {
            return Err(io::Error::other("lost manual ownership"));
        }
        if self.current == Some(raw) {
            return Ok(false);
        }
        if self.io.read(0x4f)? & MANUAL == 0 {
            return Err(io::Error::other("lost manual ownership"));
        }
        self.io.write(0x4b, raw)?;
        if self.io.read(0x4b)? != raw {
            return Err(io::Error::other("duty readback mismatch"));
        }
        self.current = Some(raw);
        Ok(true)
    }
    pub fn verify(&self) -> io::Result<()> {
        if self.io.read(0x4a)? != PWM_PERIOD || self.io.read(0x4f)? & MANUAL == 0 {
            return Err(io::Error::other("EC mode or period changed"));
        }
        if self
            .current
            .is_some_and(|v| self.io.read(0x4b).ok() != Some(v))
        {
            return Err(io::Error::other("EC target changed unexpectedly"));
        }
        Ok(())
    }
    pub fn feedback(&self) -> io::Result<(u8, Option<u16>)> {
        let temp = self.io.read(0x70)?;
        let high = self.io.read(0x76)?;
        let low = self.io.read(0x77)?;
        let high_again = self.io.read(0x76)?;
        Ok((
            temp,
            (high == high_again).then_some((u16::from(high) << 8) | u16::from(low)),
        ))
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.may_be_manual {
            return Ok(());
        }
        restore_auto(&mut self.io)?;
        self.may_be_manual = false;
        self.current = None;
        Ok(())
    }
    pub fn may_be_manual(&self) -> bool {
        self.may_be_manual
    }
    pub fn current_raw(&self) -> Option<u8> {
        self.current
    }
}
impl<T: EcIo> Drop for Fan<T> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!("EC restore failed in Drop: {e}");
        }
    }
}

pub fn restore_auto<T: EcIo>(io: &mut T) -> io::Result<()> {
    let flags = io.read(0x4f)?;
    if flags & MANUAL != 0 {
        io.write(0x4f, flags & !MANUAL)?;
    }
    if io.read(0x4f)? & MANUAL != 0 {
        return Err(io::Error::other("manual remains after restore"));
    }
    Ok(())
}

/// Do not ramp through untested sub-35% start duties. Other ramps are gradual.
pub fn ramp(current: u8, target: u8, emergency: bool) -> u8 {
    if emergency || current == 0 || target == 0 {
        return target;
    }
    if target > current {
        current.saturating_add(2).min(target)
    } else {
        current.saturating_sub(1).max(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Fake(Rc<RefCell<State>>);
    struct State {
        values: [u8; 256],
        writes: Vec<(u8, u8)>,
        fail_after_manual_write: bool,
        duty_at_takeover: Option<u8>,
    }
    impl Fake {
        fn new() -> Self {
            let mut values = [0; 256];
            values[0x4a] = 91;
            values[0x4f] = 0x05;
            Self(Rc::new(RefCell::new(State {
                values,
                writes: vec![],
                fail_after_manual_write: false,
                duty_at_takeover: None,
            })))
        }
    }
    impl EcIo for Fake {
        fn read(&self, r: u8) -> io::Result<u8> {
            Ok(self.0.borrow().values[r as usize])
        }
        fn write(&mut self, r: u8, v: u8) -> io::Result<()> {
            let mut s = self.0.borrow_mut();
            s.values[r as usize] = v;
            s.writes.push((r, v));
            if r == 0x4f
                && v & MANUAL != 0
                && let Some(duty) = s.duty_at_takeover
            {
                s.values[0x4b] = duty;
            }
            if r == 0x4f && v & MANUAL != 0 && s.fail_after_manual_write {
                s.fail_after_manual_write = false;
                return Err(io::Error::other("write accepted but reported error"));
            }
            Ok(())
        }
    }
    #[test]
    fn activation_latches_flag_first_and_steady_target_is_not_rewritten() {
        let io = Fake::new();
        let mut f = Fan::new(io.clone());
        f.activate().unwrap();
        assert!(f.apply(32).unwrap());
        assert!(!f.apply(32).unwrap());
        f.restore().unwrap();
        assert_eq!(
            io.0.borrow().writes,
            vec![(0x4f, 0x45), (0x4b, 32), (0x4f, 0x05)]
        );
    }
    #[test]
    fn indeterminate_activation_write_still_restores() {
        let io = Fake::new();
        io.0.borrow_mut().fail_after_manual_write = true;
        {
            let mut f = Fan::new(io.clone());
            assert!(f.activate().is_err());
        }
        assert_eq!(io.0.borrow().values[0x4f], 0x05);
    }
    #[test]
    fn takeover_preserves_running_firmware_duty_without_a_pwm_write() {
        let io = Fake::new();
        io.0.borrow_mut().values[0x4b] = MAX_RAW;
        let mut fan = Fan::new(io.clone());
        fan.activate().unwrap();
        assert_eq!(fan.current_raw(), Some(MAX_RAW));
        assert!(!fan.apply(MAX_RAW).unwrap());
        assert_eq!(io.0.borrow().values[0x4b], MAX_RAW);
        assert_eq!(io.0.borrow().writes, [(0x4f, 0x45)]);
    }
    #[test]
    fn takeover_acknowledges_the_last_firmware_update_and_rejects_unknown_duty() {
        for duty in [MAX_RAW, 1] {
            let io = Fake::new();
            io.0.borrow_mut().duty_at_takeover = Some(duty);
            let mut fan = Fan::new(io.clone());
            if duty == MAX_RAW {
                fan.activate().unwrap();
                assert_eq!(fan.current_raw(), Some(MAX_RAW));
                assert!(!fan.apply(MAX_RAW).unwrap());
            } else {
                assert!(fan.activate().is_err());
                assert!(fan.may_be_manual());
            }
            fan.restore().unwrap();
            assert_eq!(io.0.borrow().writes, [(0x4f, 0x45), (0x4f, 0x05)]);
        }
    }
    #[test]
    fn foreign_manual_is_never_cleared_by_drop() {
        let io = Fake::new();
        io.0.borrow_mut().values[0x4f] |= MANUAL;
        {
            let mut f = Fan::new(io.clone());
            assert!(f.activate().is_err());
        }
        assert!(io.0.borrow().writes.is_empty());
    }
    #[test]
    fn reject_unsafe_duty_and_detect_overwritten_target() {
        let io = Fake::new();
        let mut f = Fan::new(io.clone());
        f.activate().unwrap();
        for raw in [1, 23, 31, 75, 91, 255] {
            assert!(f.apply(raw).is_err());
        }
        f.apply(32).unwrap();
        io.0.borrow_mut().values[0x4b] = 46;
        assert!(f.verify().is_err());
    }
    #[test]
    fn starts_at_validated_floor_and_ramps_only_above_it() {
        assert_eq!(ramp(0, 32, false), 32);
        assert_eq!(ramp(32, 37, false), 34);
        assert_eq!(ramp(46, 32, false), 45);
        assert_eq!(ramp(32, 0, false), 0);
        assert_eq!(ramp(0, 74, true), 74);
    }
}
