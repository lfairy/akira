#![feature(coerce_unsized)]
#![feature(question_mark)]
#![feature(unique)]
#![feature(unsize)]
#![no_std]

//! This crate provides a high-level interface to UEFI.
//!
//! A typical OS loader would proceed as follows:
//!
//! 1. Call `efi::init()`
//! 2. Perform any OS-specific initialization
//! 3. Get the final memory map with `BootServices::memory_map()`
//! 4. Finally, call `BootServices::exit_boot_services()` to finish the pre-boot
//!    process

pub extern crate efi_sys as sys;

use core::fmt;
use core::iter;
use core::marker::{PhantomData, Unsize};
use core::mem;
use core::ops::{CoerceUnsized, Deref, DerefMut};
use core::ptr::{self, Unique};


// TODO: make this more typed
pub type Error = sys::Status;

pub type EfiResult<T> = Result<T, Error>;

/// Converts a low-level `EFI_STATUS` to a high-level `EfiResult`.
///
/// This returns `Ok` if the high (error) bit is not set, and `Err` otherwise.
pub fn check_status(status: sys::Status) -> EfiResult<()> {
    // TODO: handle warnings
    if status & sys::MAX_BIT == 0 {
        Ok(())
    } else {
        Err(status)
    }
}


#[derive(Copy, Clone, PartialEq)]
enum State { Boot, Runtime }

static mut INSTANCE: Option<(sys::Handle, *const sys::SystemTable)> = None;
static mut STATE: State = State::Boot;


/// Initializes the UEFI wrapper.
///
/// You should call this function *once* at the beginning of your application,
/// and share the resulting object throughout the program.
///
/// # Panics
///
/// Panics if this method is called more than once.
///
/// # Safety
///
/// This is unsafe, because the user must ensure that the arguments are valid
/// and not null.
pub unsafe fn init(
    image_handle: sys::Handle,
    system_table: *const sys::SystemTable) -> (BootServices, RuntimeServices)
{
    if INSTANCE.is_some() {
        panic!("efi::init() cannot be called more than once");
    }
    INSTANCE = Some((image_handle, system_table));
    let boot_services = BootServices {
        image_handle: image_handle,
        system_table: system_table,
    };
    let runtime_services = RuntimeServices {
        system_table: system_table,
    };
    (boot_services, runtime_services)
}


/// UEFI boot services.
///
/// A UEFI system has two modes: *boot* mode and *runtime* mode. In boot mode,
/// the firmware is in control of the system; in return, the client has access
/// to *boot services* provided by the firmware. To load an OS, the client must
/// transition to runtime mode by calling `exit_boot_services()`. As its name
/// implies, this call disables boot services and hands control of the system
/// to the client.
pub struct BootServices {
    #[allow(dead_code)] image_handle: sys::Handle,
    system_table: *const sys::SystemTable,
}

impl BootServices {
    /// Returns a handle to the console output.
    pub fn stdout(&self) -> SimpleTextOutput {
        unsafe { SimpleTextOutput::new((*self.system_table).con_out) }
    }

    /// Returns a handle to the console standard error.
    pub fn stderr(&self) -> SimpleTextOutput {
        unsafe { SimpleTextOutput::new((*self.system_table).std_err) }
    }

    /// Places an object on the UEFI heap.
    ///
    /// Panics if the allocation fails.
    pub fn boxed<T>(&self, value: T) -> EfiBox<T> {
        unsafe {
            let ptr = self.allocate(mem::size_of::<T>()) as *mut T;
            ptr::write(ptr, value);
            EfiBox::from_raw(ptr)
        }
    }

    /// Allocates a block of memory using the UEFI allocator. The memory is of
    /// type `EfiLoaderData`.
    ///
    /// Panics if the allocation fails.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory is freed (using `deallocate()`)
    /// before exiting boot services.
    pub unsafe fn allocate(&self, size: usize) -> *mut u8 {
        let mut buffer = ptr::null_mut() as *mut u8;
        let result = ((*(*self.system_table).boot_services).allocate_pool)(
                sys::MemoryType::LoaderData,
                size,
                &mut buffer as *mut _ as *mut _);
        check_status(result).unwrap();
        buffer
    }

    /// Deallocates a block of memory provided by `allocate()`.
    pub unsafe fn deallocate(&self, buffer: *mut u8) {
        // Ignore the result, since nobody checks the result of free() anyway
        let _ = ((*(*self.system_table).boot_services).free_pool)(buffer as *mut _);
    }

    /// Retrieves a copy of the UEFI memory map.
    pub fn memory_map(&self) -> MemoryMap {
        unsafe {
            let mut memory_map_size: usize = 0;
            let mut map_key: usize = 0;
            let mut descriptor_size: usize = 0;
            let mut descriptor_version: u32 = 0;
            // First, make a call with a null buffer to get its size
            let _ = ((*(*self.system_table).boot_services).get_memory_map)(
                &mut memory_map_size as *mut _,
                ptr::null_mut() as *mut _,
                &mut map_key as *mut _,
                &mut descriptor_size as *mut _,
                &mut descriptor_version as *mut _);
            // Add some wiggle room to account for the extra allocation
            // The '4' is completely arbitrary, but it tends to work in practice
            memory_map_size += 4 * descriptor_size;
            // Now call it again with an actual buffer
            let memory_map = self.allocate(memory_map_size) as *mut _;
            let result = ((*(*self.system_table).boot_services).get_memory_map)(
                &mut memory_map_size as *mut _,
                memory_map,
                &mut map_key as *mut _,
                &mut descriptor_size as *mut _,
                &mut descriptor_version as *mut _);
            match check_status(result) {
                Ok(..) => MemoryMap::from_raw(memory_map, memory_map_size, descriptor_size),
                Err(e) => {
                    self.deallocate(memory_map as *mut _);
                    panic!("Could not get memory map (error {:?})", e);
                },
            }
        }
    }

    /// Invokes the given callback with a reference to a current live
    /// `BootServices` object. Returns the result of the callback.
    ///
    /// If there is no current `BootServices` object, the callback is ignored
    /// and `None` is returned instead.
    ///
    /// This method is useful for writing panic handlers, since they don't have
    /// direct access to the system table.
    ///
    /// # Examples
    ///
    /// ```rust
    /// #[lang = "panic_fmt"]
    /// fn panic_fmt(args: core::fmt::Arguments, file: &str, line: u32) -> ! {
    ///     // Log the panic to the console
    ///     let _ = BootServices::with_instance(|bs| {
    ///         write!(bs.stderr(), "PANIC: {} {} {}\r\n", args, file, line)
    ///     });
    ///     loop {}
    /// }
    /// ```
    pub fn with_instance<F, T>(callback: F) -> Option<T> where
        F: FnOnce(&BootServices) -> T
    {
        if unsafe { STATE } != State::Boot {
            return None;
        }
        unsafe { INSTANCE }.map(|(image_handle, system_table)| {
            let bs = BootServices {
                image_handle: image_handle,
                system_table: system_table,
            };
            let result = callback(&bs);
            mem::forget(bs);
            result
        })
    }
}


/// An object allocated on the UEFI heap.
///
/// # Leaks
///
/// If you drop an `EfiBox` after exiting boot services, the backing memory will
/// be leaked. While this behavior is safe, it is usually not what you want.
pub struct EfiBox<T: ?Sized> { ptr: Unique<T> }

impl<T: ?Sized> EfiBox<T> {
    /// Constructs an `EfiBox` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The pointer must originate from the UEFI allocator itself. If not, this
    /// call may result in undefined behavior.
    pub unsafe fn from_raw(ptr: *mut T) -> Self {
        EfiBox { ptr: Unique::new(ptr) }
    }

    /// Extracts the raw pointer from an `EfiBox`.
    ///
    /// The user is then responsible for freeing the underlying memory.
    pub fn into_raw(self) -> *mut T {
        *self.ptr
    }
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<EfiBox<U>> for EfiBox<T> {}

impl<T: ?Sized> Deref for EfiBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { self.ptr.get() }
    }
}

impl<T: ?Sized> DerefMut for EfiBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.get_mut() }
    }
}

impl<T: ?Sized> Drop for EfiBox<T> {
    fn drop(&mut self) {
        unsafe { ptr::drop_in_place(*self.ptr); }
        BootServices::with_instance(|bs| unsafe { bs.deallocate(*self.ptr as *mut _) });
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for EfiBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: fmt::Display + ?Sized> fmt::Display for EfiBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}


/// Provides a simple interface for displaying text.
pub struct SimpleTextOutput<'e> {
    out: *const sys::SimpleTextOutputProtocol,
    _marker: PhantomData<&'e BootServices>,
}

impl<'e> SimpleTextOutput<'e> {
    /// Constructs a `SimpleTextOutput` from a raw protocol handle.
    ///
    /// This is a low-level constructor: you most likely want to use
    /// `BootServices::stdout()` or `BootServices::stderr()` instead.
    ///
    /// # Safety
    ///
    /// This is unsafe because the user must check that the handle points to a
    /// valid object. Also, the user must ensure that the `SimpleTextOutput` is
    /// dropped before exiting boot services.
    pub unsafe fn new(out: *const sys::SimpleTextOutputProtocol) -> SimpleTextOutput<'e> {
        SimpleTextOutput {
            out: out,
            _marker: PhantomData,
        }
    }

    /// Write a string to the handle.
    pub fn write_str(&self, s: &str) -> EfiResult<()> {
        let mut buffer = [0u16; 128];
        let mut chars = s.chars().peekable();
        while chars.peek().is_some() {
            let chunk = chars.by_ref().take(buffer.len()-1).chain(iter::once('\0'));
            for (d, c) in buffer.iter_mut().zip(chunk) {
                *d = c as u16;  // UCS-2
            }
            let status = unsafe {
                ((*self.out).output_string)(self.out, buffer.as_ptr())
            };
            check_status(status)?;
        }
        Ok(())
    }

    /// Write a formatting object to the handle.
    ///
    /// This method lets you use the `write!` macro to output formatted text.
    ///
    /// # Examples
    ///
    /// ```rust
    /// let out = bs.stdout();
    /// write!(out, "Hello, world!\r\n").unwrap();
    /// ```
    pub fn write_fmt(&self, args: fmt::Arguments) -> EfiResult<()> {
        struct Writer<'e> {
            inner: &'e SimpleTextOutput<'e>,
            result: EfiResult<()>,
        }
        impl<'e> fmt::Write for Writer<'e> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.result = self.inner.write_str(s);
                self.result.map_err(|_| fmt::Error)
            }
        }
        let mut writer = Writer { inner: self, result: Ok(()) };
        let _ = fmt::Write::write_fmt(&mut writer, args);
        writer.result
    }
}


/// Represents a UEFI memory map.
///
/// Internally, this is an array of `MemoryDescriptor` values and supports the
/// usual operations (indexing/iteration).
#[derive(Debug)]
pub struct MemoryMap {
    ptr: *mut sys::MemoryDescriptor,
    memory_map_size: usize,
    descriptor_size: usize,
}

impl MemoryMap {
    /// Constructs a memory map.
    ///
    /// This takes ownership of the underlying buffer, and will deallocate it
    /// on drop.
    ///
    /// This is a low-level constructor: you most likely want to use
    /// `BootServices::memory_map()` instead.
    ///
    /// # Panics
    ///
    /// Panics if:
    ///
    /// * The pointer is null
    /// * The descriptor size is zero
    /// * The memory map size is not a multiple of the descriptor size
    ///
    /// # Safety
    ///
    /// This is unsafe because the arguments must specify a valid memory map.
    pub unsafe fn from_raw(
        ptr: *mut sys::MemoryDescriptor,
        memory_map_size: usize,
        descriptor_size: usize) -> Self
    {
        assert!(!ptr.is_null());
        assert!(descriptor_size > 0);
        assert!(memory_map_size % descriptor_size == 0);
        MemoryMap {
            ptr: ptr,
            memory_map_size: memory_map_size,
            descriptor_size: descriptor_size,
        }
    }

    /// Returns the number of entries in the memory map.
    pub fn len(&self) -> usize {
        self.memory_map_size / self.descriptor_size
    }

    /// Returns an iterator over the memory map.
    pub fn iter(&self) -> MemoryMapIter {
        MemoryMapIter {
            _marker: PhantomData,
            ptr: self.ptr,
            memory_map_size: self.memory_map_size,
            descriptor_size: self.descriptor_size,
        }
    }

    /// Returns a mutable iterator over the memory map.
    pub fn iter_mut(&mut self) -> MemoryMapMutIter {
        MemoryMapMutIter {
            _marker: PhantomData,
            ptr: self.ptr,
            memory_map_size: self.memory_map_size,
            descriptor_size: self.descriptor_size,
        }
    }
}

impl Drop for MemoryMap {
    fn drop(&mut self) {
        BootServices::with_instance(|bs| unsafe { bs.deallocate(self.ptr as *mut _) });
    }
}

impl<'a> IntoIterator for &'a MemoryMap {
    type Item = &'a MemoryDescriptor;
    type IntoIter = MemoryMapIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut MemoryMap {
    type Item = &'a mut MemoryDescriptor;
    type IntoIter = MemoryMapMutIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

pub use sys::{MemoryDescriptor, MemoryType, PhysicalAddress, VirtualAddress, MemoryAttribute};

/// Iterator for `MemoryMap`.
#[derive(Debug)]
pub struct MemoryMapIter<'a> {
    _marker: PhantomData<&'a MemoryMap>,
    ptr: *mut sys::MemoryDescriptor,
    memory_map_size: usize,
    descriptor_size: usize,
}

impl<'a> Iterator for MemoryMapIter<'a> {
    type Item = &'a MemoryDescriptor;
    fn next(&mut self) -> Option<Self::Item> {
        if self.memory_map_size == 0 {
            None
        } else {
            unsafe {
                let result = &*self.ptr;
                self.ptr = (self.ptr as *mut u8).offset(self.descriptor_size as isize) as *mut _;
                self.memory_map_size -= self.descriptor_size;
                Some(result)
            }
        }
    }
}

/// Iterator for `MemoryMap`.
#[derive(Debug)]
pub struct MemoryMapMutIter<'a> {
    _marker: PhantomData<&'a MemoryMap>,
    ptr: *mut sys::MemoryDescriptor,
    memory_map_size: usize,
    descriptor_size: usize,
}

impl<'a> Iterator for MemoryMapMutIter<'a> {
    type Item = &'a mut MemoryDescriptor;
    fn next(&mut self) -> Option<Self::Item> {
        if self.memory_map_size == 0 {
            None
        } else {
            unsafe {
                let result = &mut *self.ptr;
                self.ptr = (self.ptr as *mut u8).offset(self.descriptor_size as isize) as *mut _;
                self.memory_map_size -= self.descriptor_size;
                Some(result)
            }
        }
    }
}


/// UEFI runtime services.
///
/// This struct contains methods that are available in both boot mode and
/// runtime mode.
pub struct RuntimeServices {
    #[allow(dead_code)] system_table: *const sys::SystemTable,
}
