//! Fences: a 64-bit value and the events or callers waiting for it.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{collections::BTreeMap, ffi::c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{self, E_INVALIDARG, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 4] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Fence {
    object: Object,
    /// Titles also read it straight from memory, at the fence's GPU
    /// address.
    value: AtomicU64,
    flags: u32,
    waiters: Mutex<Vec<(u64, Waiter)>>,
}

enum Waiter {
    Event(usize),
    Blocking(std::sync::mpsc::Sender<()>),
}

/// Fences whose addresses the title can write from a command list.
static GPU_ADDRESSES: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

impl Drop for Fence {
    fn drop(&mut self) {
        let mut addresses = GPU_ADDRESSES.lock().unwrap_or_else(|e| e.into_inner());
        addresses.remove(&(&raw const self.value as usize));
    }
}

fn fence<'a>(this: This) -> &'a Fence {
    // SAFETY: called from fence methods only.
    unsafe { &*this.cast::<Fence>() }
}

pub(crate) extern "system" fn create(
    _: This,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::debug!(initial, flags, "fence");
    let fence = Box::new(Fence {
        object: Object::new(&FUNCTIONS),
        value: AtomicU64::new(initial),
        flags,
        waiters: Mutex::new(Vec::new()),
    });
    object::hand_out(FENCE, fence, iid, &FENCE_IIDS, out)
}

fn set_event(event: usize) {
    #[cfg(windows)]
    // SAFETY: the title passed an event handle.
    unsafe {
        windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void)
    };
    #[cfg(not(windows))]
    let _ = event;
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        FENCE,
        iid,
        object::is(iid, &FENCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Fence>(this)
}
pub(crate) extern "system" fn completed(this: This) -> u64 {
    fence(this).value.load(Ordering::Acquire)
}
extern "system" fn set_event_on_completion(this: This, value: u64, event: *mut c_void) -> HRESULT {
    let fence = fence(this);
    if event.is_null() {
        let receiver = {
            let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
            if fence.value.load(Ordering::Acquire) >= value {
                return S_OK;
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            waiters.push((value, Waiter::Blocking(sender)));
            receiver
        };
        // Settling can signal this fence, so do it after releasing its lock.
        #[cfg(windows)]
        crate::host::settle_for_fence();
        return if receiver.recv().is_ok() { S_OK } else { E_INVALIDARG };
    }
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    if fence.value.load(Ordering::Acquire) >= value {
        set_event(event as usize);
    } else {
        waiters.push((value, Waiter::Event(event as usize)));
    }
    S_OK
}
/// Moves the fence to `value` and sets the events that waited for it.
pub(crate) fn set(this: This, value: u64) {
    update(this, |_| value);
}

fn update(this: This, value: impl FnOnce(u64) -> u64) {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    let value = value(fence.value.load(Ordering::Relaxed));
    fence.value.store(value, Ordering::Release);
    waiters.retain(|(wanted, waiter)| {
        if *wanted > value {
            return true;
        }
        match waiter {
            Waiter::Event(event) => set_event(*event),
            Waiter::Blocking(sender) => { let _ = sender.send(()); }
        }
        false
    });
}

/// A bottom-of-pipe write into a fence updates it atomically and wakes its
/// waiters. Dword writes preserve the other half of the value.
pub(crate) fn write_gpu_address(address: usize, bytes: usize, value: u64) -> bool {
    let base = address & !7;
    let shift = ((address - base) * 8) as u32;
    let mask = match (bytes, shift) {
        (8, 0) => u64::MAX,
        (4, 0 | 32) => u64::from(u32::MAX) << shift,
        _ => return false,
    };
    let this = {
        let addresses = GPU_ADDRESSES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(&this) = addresses.get(&base) else { return false };
        let this = this as This;
        if !object::try_add_ref(this) {
            tracing::warn!(address, "GPU write targeted a fence being destroyed");
            return true;
        }
        this
    };
    update(this, |old| (old & !mask) | ((value << shift) & mask));
    release(this);
    true
}
pub(crate) extern "system" fn signal(this: This, value: u64) -> HRESULT {
    set(this, value);
    // A queue may have waited for it: what it held runs now.
    #[cfg(windows)]
    crate::queue::release_held(false);
    S_OK
}

extern "system" fn creation_flags(this: This) -> u32 {
    fence(this).flags
}
/// Where the value lives (GPU addresses are CPU addresses).
extern "system" fn gpu_address(this: This) -> u64 {
    let address = &raw const fence(this).value as usize;
    let mut addresses = GPU_ADDRESSES.lock().unwrap_or_else(|e| e.into_inner());
    addresses.insert(address, this as usize);
    address as u64
}

/// IUnknown 3, object 4, device child 1, fence 3, fence 1 (2, a console
/// one among them).
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<FENCE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = completed as *const (); // 0x40
    f[9] = set_event_on_completion as *const (); // 0x48
    f[10] = signal as *const (); // 0x50
    f[11] = creation_flags as *const (); // 0x58
    f[12] = gpu_address as *const (); // 0x60
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f), S_OK);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        // SAFETY: plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let wait = slot::<extern "system" fn(This, u64, *mut c_void) -> HRESULT>(f, 0x48);
        let signal = slot::<extern "system" fn(This, u64) -> HRESULT>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        // SAFETY: a valid event.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(signal(f, 5), S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, std::ptr::null_mut()), S_OK);
        // The title reads the value at the fence's address.
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f);
        // SAFETY: the fence's value.
        assert_eq!(unsafe { *(at as *const u64) }, 5);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_bottom_of_pipe_value_write_signals_fence_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1 << 32, 0, &FENCE_IIDS[0], &mut f), S_OK);
        // SAFETY: a plain auto-reset event used only in this test.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let address = gpu_address(f) as *mut u8;
        assert_eq!(set_event_on_completion(f, (1 << 32) + 7, event), S_OK);
        crate::queue::Written::Value(6).write(address, 4);
        assert_eq!(completed(f), (1 << 32) + 6);
        // SAFETY: valid event until CloseHandle below.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_TIMEOUT);
        crate::queue::Written::Value(7).write(address, 4);
        assert_eq!(completed(f), (1 << 32) + 7);
        // SAFETY: valid event until CloseHandle below.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        crate::queue::Written::Value(11).write(address, 8);
        assert_eq!(completed(f), 11);
        // SAFETY: this test owns the event.
        assert_ne!(unsafe { CloseHandle(event) }, 0);
        assert_eq!(release(f), 0);
        assert!(!write_gpu_address(address as usize, 8, 0));
    }

    #[test]
    fn null_event_waits_for_a_signal_on_another_thread() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f), S_OK);
        let address = f as usize;
        let (returned, result) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let status = set_event_on_completion(address as This, 9, std::ptr::null_mut());
            returned.send(status).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while fence(f).waiters.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "blocking wait did not register");
            std::thread::yield_now();
        }
        set(f, 8);
        assert_eq!(result.try_recv(), Err(mpsc::TryRecvError::Empty));
        set(f, 9);
        // The call must succeed even if the fence is rewound before the
        // waiting thread is scheduled again.
        set(f, 1);
        assert_eq!(result.recv_timeout(Duration::from_secs(2)), Ok(S_OK));
        waiter.join().unwrap();
        assert_eq!(release(f), 0);
    }
}
