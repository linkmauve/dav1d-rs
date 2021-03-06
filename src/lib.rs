use dav1d_sys::*;

use std::ffi::c_void;
use std::fmt;
use std::i64;
use std::mem;
use std::ptr;
use std::sync::Arc;

#[derive(Debug)]
pub struct Error(i32);

impl Error {
    const fn is_again(&self) -> bool {
        const AGAIN: i32 = EAGAIN as i32;
        if AGAIN < 0 {
            self.0 == AGAIN
        } else {
            self.0 == -AGAIN
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
pub struct Decoder {
    dec: *mut Dav1dContext,
}

unsafe extern "C" fn release_wrapped_data(_data: *const u8, cookie: *mut c_void) {
    let closure: &mut &mut dyn FnMut() = &mut *(cookie as *mut &mut dyn std::ops::FnMut());
    closure();
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        unsafe {
            let mut settings = mem::MaybeUninit::uninit();
            let mut dec = mem::MaybeUninit::uninit();

            dav1d_default_settings(settings.as_mut_ptr());

            let settings = settings.assume_init();

            let ret = dav1d_open(dec.as_mut_ptr(), &settings);

            if ret != 0 {
                panic!("Cannot instantiate the default decoder {}", ret);
            }

            Decoder {
                dec: dec.assume_init(),
            }
        }
    }

    pub fn flush(&self) {
        unsafe {
            dav1d_flush(self.dec);
        }
    }

    pub fn send_data<T: AsRef<[u8]>>(
        &mut self,
        buf: T,
        offset: Option<i64>,
        timestamp: Option<i64>,
        duration: Option<i64>,
    ) -> Result<(), Error> {
        let buf = buf.as_ref();
        let len = buf.len();
        unsafe {
            let mut data: Dav1dData = mem::zeroed();
            let ptr = dav1d_data_create(&mut data, len);
            ptr::copy_nonoverlapping(buf.as_ptr(), ptr, len);
            if let Some(offset) = offset {
                data.m.offset = offset;
            }
            if let Some(timestamp) = timestamp {
                data.m.timestamp = timestamp;
            }
            if let Some(duration) = duration {
                data.m.duration = duration;
            }
            let ret = dav1d_send_data(self.dec, &mut data);
            if ret < 0 {
                Err(Error(ret))
            } else {
                Ok(())
            }
        }
    }

    pub fn get_picture(&mut self) -> Result<Picture, Error> {
        unsafe {
            let mut pic: Dav1dPicture = mem::zeroed();
            let ret = dav1d_get_picture(self.dec, &mut pic);

            if ret < 0 {
                Err(Error(ret))
            } else {
                let inner = InnerPicture { pic };
                Ok(Picture {
                    inner: Arc::new(inner),
                })
            }
        }
    }

    pub fn decode<T: AsRef<[u8]>, F: FnMut()>(
        &mut self,
        buf: T,
        offset: Option<i64>,
        timestamp: Option<i64>,
        duration: Option<i64>,
        mut destroy_notify: F,
    ) -> Result<Vec<Picture>, Error> {
        let buf = buf.as_ref();
        let len = buf.len();
        unsafe {
            let mut data: Dav1dData = mem::zeroed();
            let mut cb: &mut dyn FnMut() = &mut destroy_notify;
            let cb = &mut cb;
            let _ret = dav1d_data_wrap(
                &mut data,
                buf.as_ptr(),
                len,
                Some(release_wrapped_data),
                cb as *mut _ as *mut c_void,
            );
            if let Some(offset) = offset {
                data.m.offset = offset;
            }
            if let Some(timestamp) = timestamp {
                data.m.timestamp = timestamp;
            }
            if let Some(duration) = duration {
                data.m.duration = duration;
            }
            let mut pictures: Vec<Picture> = Vec::new();
            while data.sz > 0 {
                let ret = dav1d_send_data(self.dec, &mut data);
                let err = Error(ret);
                if ret < 0 && !err.is_again() {
                    return Err(err);
                }
                match self.get_picture() {
                    Ok(p) => pictures.push(p),
                    Err(e) => {
                        if e.is_again() {
                            continue;
                        } else {
                            break;
                        }
                    }
                };
            }
            Ok(pictures)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { dav1d_close(&mut self.dec) };
    }
}

unsafe impl Send for Decoder {}

#[derive(Debug)]
struct InnerPicture {
    pub pic: Dav1dPicture,
}

#[derive(Debug, Clone)]
pub struct Picture {
    inner: Arc<InnerPicture>,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum PixelLayout {
    I400,
    I420,
    I422,
    I444,
    Unknown,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
pub enum PlanarImageComponent {
    Y,
    U,
    V,
}

impl std::convert::From<usize> for PlanarImageComponent {
    fn from(index: usize) -> Self {
        match index {
            0 => PlanarImageComponent::Y,
            1 => PlanarImageComponent::U,
            2 => PlanarImageComponent::V,
            _ => panic!("Invalid YUV index: {}", index),
        }
    }
}

impl std::convert::From<PlanarImageComponent> for usize {
    fn from(component: PlanarImageComponent) -> Self {
        match component {
            PlanarImageComponent::Y => 0,
            PlanarImageComponent::U => 1,
            PlanarImageComponent::V => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plane(Picture, PlanarImageComponent);

impl AsRef<[u8]> for Plane {
    fn as_ref(&self) -> &[u8] {
        let (stride, height) = self.0.plane_data_geometry(self.1);
        unsafe {
            std::slice::from_raw_parts(
                self.0.plane_data_ptr(self.1) as *const u8,
                (stride * height) as usize,
            )
        }
    }
}
unsafe impl Send for Plane {}
unsafe impl Sync for Plane {}

#[derive(Copy, Clone, Debug)]
pub struct BitsPerComponent(pub usize);

impl Picture {
    pub fn stride(&self, component: PlanarImageComponent) -> u32 {
        let s = match component {
            PlanarImageComponent::Y => 0,
            _ => 1,
        };
        (*self.inner).pic.stride[s] as u32
    }

    pub fn plane_data_ptr(&self, component: PlanarImageComponent) -> *mut c_void {
        let index: usize = component.into();
        (*self.inner).pic.data[index]
    }

    pub fn plane_data_geometry(&self, component: PlanarImageComponent) -> (u32, u32) {
        let height = match component {
            PlanarImageComponent::Y => self.height(),
            _ => match self.pixel_layout() {
                PixelLayout::I420 => (self.height() + 1) / 2,
                PixelLayout::I400 | PixelLayout::I422 | PixelLayout::I444 => self.height(),
                PixelLayout::Unknown => unreachable!(),
            },
        };
        (self.stride(component) as u32, height)
    }

    pub fn plane(&self, component: PlanarImageComponent) -> Plane {
        Plane(self.clone(), component)
    }

    pub fn bit_depth(&self) -> usize {
        (*self.inner).pic.p.bpc as usize
    }

    pub fn bits_per_component(&self) -> Option<BitsPerComponent> {
        unsafe {
            match (*(*self.inner).pic.seq_hdr).hbd {
                0 => Some(BitsPerComponent(8)),
                1 => Some(BitsPerComponent(10)),
                2 => Some(BitsPerComponent(12)),
                _ => None,
            }
        }
    }

    pub fn width(&self) -> u32 {
        (*self.inner).pic.p.w as u32
    }

    pub fn height(&self) -> u32 {
        (*self.inner).pic.p.h as u32
    }

    pub fn pixel_layout(&self) -> PixelLayout {
        #[allow(non_upper_case_globals)]
        match (*self.inner).pic.p.layout {
            Dav1dPixelLayout_DAV1D_PIXEL_LAYOUT_I400 => PixelLayout::I400,
            Dav1dPixelLayout_DAV1D_PIXEL_LAYOUT_I420 => PixelLayout::I420,
            Dav1dPixelLayout_DAV1D_PIXEL_LAYOUT_I422 => PixelLayout::I422,
            Dav1dPixelLayout_DAV1D_PIXEL_LAYOUT_I444 => PixelLayout::I444,
            _ => PixelLayout::Unknown,
        }
    }

    pub fn timestamp(&self) -> Option<i64> {
        let ts = (*self.inner).pic.m.timestamp;
        if ts == i64::MIN {
            None
        } else {
            Some(ts)
        }
    }

    pub fn duration(&self) -> i64 {
        (*self.inner).pic.m.duration as i64
    }

    pub fn offset(&self) -> i64 {
        (*self.inner).pic.m.offset
    }
}

unsafe impl Send for Picture {}
unsafe impl Sync for Picture {}

impl Drop for InnerPicture {
    fn drop(&mut self) {
        unsafe {
            dav1d_picture_unref(&mut self.pic);
        }
    }
}

pub fn parse_sequence_header<T: AsRef<[u8]>>(buf: T) -> Result<SequenceHeader, Error> {
    let buf = buf.as_ref();
    let len = buf.len();
    unsafe {
        let mut seq: Dav1dSequenceHeader = mem::zeroed();
        let ret = dav1d_parse_sequence_header(&mut seq, buf.as_ptr(), len);
        if ret < 0 {
            Err(Error(ret))
        } else {
            Ok(SequenceHeader { seq: Arc::new(seq) })
        }
    }
}

#[derive(Debug)]
pub struct SequenceHeader {
    seq: Arc<Dav1dSequenceHeader>,
}

impl SequenceHeader {}

impl Drop for SequenceHeader {
    fn drop(&mut self) {
        Arc::get_mut(&mut self.seq).unwrap();
    }
}
