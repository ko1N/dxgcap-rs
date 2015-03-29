// The MIT License (MIT)
//
// Copyright (c) 2015 Johan Johansson
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Capture the screen with DXGI in rust

#![feature(unsafe_destructor, std_misc)]
#![allow(dead_code, non_snake_case)]

extern crate winapi;
#[macro_use(c_mtdcall)]
extern crate dxgi;
extern crate d3d11;

use std::{ mem, slice, ptr };
use std::mem::{ transmute, zeroed };
use std::time::duration::Duration;
use winapi::{ HRESULT, IID, DWORD, RECT, HMONITOR, BOOL, E_ACCESSDENIED };
use dxgi::constants::*;
use dxgi::interfaces::*;
use dxgi::{ CreateDXGIFactory1, DXGI_OUTPUT_DESC, DXGI_MODE_ROTATION };
use d3d11::constants::*;
use d3d11::interfaces::*;
use d3d11::{ D3D11_USAGE, D3D11_CPU_ACCESS_FLAG, D3D_DRIVER_TYPE,
	D3D_FEATURE_LEVEL, D3D11CreateDevice };

#[repr(C)] struct MONITORINFO {
	cbSize: DWORD,
	rcMonitor: RECT,
	rcWork: RECT,
	dwFlags: DWORD,
}

#[link(name = "user32")]
extern "C" {
	fn GetMonitorInfoW(monitor: HMONITOR, monitor_info: *mut MONITORINFO) -> BOOL;
}

pub const DXGI_PIXEL_SIZE: u32 = 4; // BGRA8 => 4 bytes, DXGI default

#[derive(Copy, Clone, Debug, PartialOrd, PartialEq, Eq, Ord)]
pub struct BGRA8 {
	pub b: u8,
	pub g: u8,
	pub r: u8,
	pub a: u8,
}

/// A unique pointer to a COM object. Handles refcounting.
pub struct UniqueCOMPtr<T: IUnknownT> {
	ptr: *mut T,
}
impl<T: IUnknownT> UniqueCOMPtr<T> {
	/// Construct a new unique COM pointer from a pointer to a COM object.
	/// It is the users responsibility to guarantee that no copies of the pointer exists beforehand 
	pub unsafe fn from_ptr(ptr: *mut T) -> UniqueCOMPtr<T> {
		UniqueCOMPtr{ ptr: ptr }
	}

	pub unsafe fn query_interface<U>(mut self, interface_identifier: &IID)
		-> Result<UniqueCOMPtr<U>, HRESULT> where U: IUnknownT
	{
		let mut interface = ptr::null_mut();
		let hr = self.QueryInterface(interface_identifier, &mut interface);
		if hr_failed(hr) {
			Err(hr)
		} else {
			Ok(UniqueCOMPtr::from_ptr(interface as *mut U))
		}
	}
}
impl<T: IUnknownT> std::ops::Deref for UniqueCOMPtr<T> {
	type Target = T;

	fn deref(&self) -> &T {
		unsafe { &*self.ptr }
	}
}
impl<T: IUnknownT> std::ops::DerefMut for UniqueCOMPtr<T> {
	fn deref_mut(&mut self) -> &mut T {
		unsafe { &mut *self.ptr }
	}
}
#[unsafe_destructor]
impl<T: IUnknownT> std::ops::Drop for UniqueCOMPtr<T> {
	fn drop(&mut self) {
		self.Release();
	}
}
/// This is not actually necessarily thread safe.
/// It's up to the user to guarantee that all pointers are uniquely owned.
unsafe impl<T> Send for UniqueCOMPtr<T> { }

/// Possible errors when capturing
#[derive(Debug)]
pub enum CaptureError {
	// Could not duplicate output, access denied. Might be in protected fullscreen.
	AccessDenied,
	// Access to the duplicated output was lost. Likely, mode was changed e.g. window => full
	AccessLost,
	// Error when trying to refresh outputs after some failure.
	RefreshFailure,
	// AcquireNextFrame timed out.
	Timeout,
	// General/Unexpected failure
	Fail(&'static str),
}

pub fn hr_failed(hr: HRESULT) -> bool { hr < 0 }

fn c_utf16_to_string(chars: &[u16]) -> String {
	String::from_utf16_lossy(
		&chars.iter().cloned().take_while(|&b| b != 0).collect::<Vec<_>>()[..])
}

fn max<T: PartialOrd>(a: T, b: T) -> T { if a > b { a } else { b } }

fn create_dxgi_factory_1() -> UniqueCOMPtr<IDXGIFactory1> {
	unsafe {
		let mut factory = ptr::null_mut();

		let hr = CreateDXGIFactory1(&IID_IDXGIFactory1, &mut factory);
		if hr_failed(hr) {
			panic!("Failed to create DXGIFactory1, {:x}", hr)
		} else {
			UniqueCOMPtr::from_ptr(factory as *mut IDXGIFactory1)
		}
	}
}

fn d3d11_create_device<T: IDXGIAdapterT>(adapter: &mut T)
	-> (UniqueCOMPtr<ID3D11Device>, UniqueCOMPtr<ID3D11DeviceContext>)
{
	unsafe {
		let (mut d3d11_device, mut device_context) = (ptr::null_mut(), ptr::null_mut());

		let hr = D3D11CreateDevice(transmute(adapter),
			D3D_DRIVER_TYPE::UNKNOWN,
			ptr::null_mut(), 0, ptr::null_mut(), 0,
			D3D11_SDK_VERSION,
			&mut d3d11_device,
			&mut D3D_FEATURE_LEVEL::FL_9_1,
			&mut device_context);
		if hr_failed(hr) {
			panic!("Failed to create d3d11 device and device context, {:x}", hr)
		} else {
			(UniqueCOMPtr::from_ptr(d3d11_device as *mut ID3D11Device),
				UniqueCOMPtr::from_ptr(device_context))
		}
	}
}

fn get_adater_outputs(adapter: &mut IDXGIAdapter1) -> Vec<UniqueCOMPtr<IDXGIOutput>> {
	(0..).map(|i| unsafe {
			let mut output = ptr::null_mut();
			if hr_failed(adapter.EnumOutputs(i, &mut output)) {
				None
			} else {
				let mut out_desc = zeroed();
				(*output).GetDesc(&mut out_desc);

				if out_desc.AttachedToDesktop != 0 {
					Some(UniqueCOMPtr::from_ptr(output))
				} else {
					None
				}
			}
		})
		.take_while(Option::is_some).map(Option::unwrap)
		.collect()
}

fn output_is_primary(output: &IDXGIOutput1) -> bool {
	unsafe {
		let mut output_desc = zeroed();
		transmute::<_, &mut IDXGIOutput1>(output).GetDesc(&mut output_desc);

		let mut monitor_info: MONITORINFO = zeroed();
		monitor_info.cbSize = mem::size_of::<MONITORINFO>() as u32;
		GetMonitorInfoW(output_desc.Monitor, &mut monitor_info);

		(monitor_info.dwFlags & 1) != 0
	}
}

fn get_capture_source(
	output_dups: Vec<(UniqueCOMPtr<IDXGIOutputDuplication>, UniqueCOMPtr<IDXGIOutput1>)>,
	cs_index: usize)
	-> Option<(UniqueCOMPtr<IDXGIOutputDuplication>, UniqueCOMPtr<IDXGIOutput1>)>
{
	if cs_index == 0 {
		output_dups.into_iter().find(|&(_, ref out)| output_is_primary(&out))
	} else {
		output_dups.into_iter()
			.filter(|&(_, ref out)| !output_is_primary(&out))
			.nth(cs_index - 1)
	}
}

fn duplicate_outputs(device: UniqueCOMPtr<ID3D11Device>, outputs: Vec<UniqueCOMPtr<IDXGIOutput>>)
	-> (UniqueCOMPtr<ID3D11Device>,
		Vec<(UniqueCOMPtr<IDXGIOutputDuplication>, UniqueCOMPtr<IDXGIOutput1>)>)
{
	unsafe {
		outputs.into_iter()
			.map(|out| out.query_interface::<IDXGIOutput1>(&IID_IDXGIOutput1).unwrap())
			.fold((device, Vec::new()), |(device, mut out_dups), mut output| {
				let mut dxgi_device =
					device.query_interface(&IID_IDXGIDevice1).unwrap();

				let output_duplication = {
					let mut output_duplication = ptr::null_mut();
					assert_eq!(0,
						output.DuplicateOutput(
							transmute::<&mut IDXGIDevice1, _>(&mut dxgi_device),
							&mut output_duplication));
					UniqueCOMPtr::from_ptr(output_duplication) };

				out_dups.push((output_duplication, output));
				(dxgi_device.query_interface(&IID_ID3D11Device).unwrap(), out_dups)
			})
	}
}

struct DuplicatedOutput {
	device: UniqueCOMPtr<ID3D11Device>,
	device_context: UniqueCOMPtr<ID3D11DeviceContext>,
	output: UniqueCOMPtr<IDXGIOutput1>,
	output_duplication: UniqueCOMPtr<IDXGIOutputDuplication>,
}
impl DuplicatedOutput {
	fn get_desc(&self) -> DXGI_OUTPUT_DESC {
		unsafe {
			let mut desc = zeroed();
			transmute::<_, &mut Self>(self).output.GetDesc(&mut desc);
			desc
		}
	}

	fn get_frame(&mut self, timeout_ms: u32) -> Result<UniqueCOMPtr<IDXGISurface1>, HRESULT> {
		let frame_resource = unsafe {
			let mut frame_resource = ptr::null_mut();
			let mut frame_info = zeroed();
			let hr = self.output_duplication
				.AcquireNextFrame(timeout_ms, &mut frame_info, &mut frame_resource);
			if hr_failed(hr) {
				return Err(hr);
			}
			UniqueCOMPtr::from_ptr(frame_resource)
		};

		let mut frame_texture: UniqueCOMPtr<ID3D11Texture2D> = unsafe {
			frame_resource.query_interface(&IID_ID3D11Texture2D).unwrap()
		};

		let mut texture_desc = unsafe { zeroed() };
		frame_texture.GetDesc(&mut texture_desc);

		// Configure the description to make the texture readable
		texture_desc.Usage = D3D11_USAGE::STAGING;
		texture_desc.BindFlags = 0;
		texture_desc.CPUAccessFlags = D3D11_CPU_ACCESS_FLAG::READ as u32;
		texture_desc.MiscFlags = 0;

		let mut readable_texture = unsafe {
			let mut readable_texture = ptr::null_mut();
			let hr = self.device
				.CreateTexture2D(&mut texture_desc, ptr::null(), &mut readable_texture);
			if hr_failed(hr) {
				return Err(hr);
			}
			UniqueCOMPtr::from_ptr(readable_texture)
		};

		// Lower priorities causes stuff to be needlessly copied from gpu to ram,
		// causing huge fluxuations on some systems.
		readable_texture.SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM);

		unsafe {
			let mut readable_surface =
				readable_texture.query_interface(&IID_ID3D11Resource).unwrap();

			self.device_context.CopyResource(&mut *readable_surface,
				&mut *frame_texture.query_interface(&IID_ID3D11Resource).unwrap());

			self.output_duplication.ReleaseFrame();

			readable_surface.query_interface(&IID_IDXGISurface1)
		}
	}

	fn release_frame(&mut self) -> Result<(), HRESULT> {
		let hr = self.output_duplication.ReleaseFrame();
		if hr_failed(hr) {
			Err(hr)
		} else {
			Ok(())
		}
	}
}

pub struct DXGIManager {
	duplicated_output: Option<DuplicatedOutput>,
	capture_source_index: usize,
	timeout_ms: u32,
}
impl DXGIManager {
	pub fn new(timeout: Duration) -> Result<DXGIManager, &'static str> {
		let mut manager = DXGIManager{ duplicated_output: None,
			capture_source_index: 0,
			timeout_ms: max(timeout.num_milliseconds(), 0) as u32 };

		match manager.acquire_output_duplication() {
			Ok(_) => Ok(manager),
			Err(_) => Err("Failed to get outputs")
		}
	}

	pub fn set_capture_source_index(&mut self, cs: usize) {
		self.capture_source_index = cs;
		self.acquire_output_duplication().unwrap()
	}

	pub fn get_capture_source_index(&self) -> usize { self.capture_source_index }

	pub fn set_timeout(&mut self, t: Duration) {
		self.timeout_ms = max(t.num_milliseconds(), 0) as u32
	}

	pub fn acquire_output_duplication(&mut self) -> Result<(), ()> {
		self.duplicated_output = None;

		let mut factory = create_dxgi_factory_1();

		for (outputs, mut adapter) in (0..).map(|i| {
				let mut adapter = ptr::null_mut();
				if factory.EnumAdapters1(i, &mut adapter) != DXGI_ERROR_NOT_FOUND {
					Some(unsafe { UniqueCOMPtr::from_ptr(adapter) })
				} else {
					None
				}
			})
			.take_while(Option::is_some).map(Option::unwrap)
			.map(|mut adapter| (get_adater_outputs(&mut adapter), adapter))
			.filter(|&(ref outs, _)| !outs.is_empty())
		{
			// Creating device for each adapter that has the output
			let (d3d11_device, device_context) = d3d11_create_device(&mut *adapter);

			let (d3d11_device, output_duplications) = duplicate_outputs(d3d11_device, outputs);

			if let Some((output_duplication, output)) =
				get_capture_source(output_duplications, self.capture_source_index)
			{
				self.duplicated_output = Some(DuplicatedOutput{
					device: d3d11_device,
					device_context: device_context,
					output: output,
					output_duplication: output_duplication
				});
				return Ok(())
			}
		}

		Err(())
	}

	fn get_frame(&mut self) -> Result<UniqueCOMPtr<IDXGISurface1>, CaptureError> {
		if let None = self.duplicated_output {
			if let Ok(_) = self.acquire_output_duplication() {
				return Err(CaptureError::Fail("No valid duplicated output"))
			} else {
				return Err(CaptureError::RefreshFailure)
			}
		}

		let timeout_ms = self.timeout_ms;

		match self.duplicated_output.as_mut().unwrap().get_frame(timeout_ms) {
			Ok(surface) => Ok(surface),
			Err(DXGI_ERROR_ACCESS_LOST) => if let Ok(_) = self.acquire_output_duplication() {
				Err(CaptureError::AccessLost)
			} else {
				Err(CaptureError::RefreshFailure) },
			Err(E_ACCESSDENIED) => Err(CaptureError::AccessDenied),
			Err(DXGI_ERROR_WAIT_TIMEOUT) => Err(CaptureError::Timeout),
			Err(_) => if let Ok(_) = self.acquire_output_duplication() {
				Err(CaptureError::Fail("Failure when acquiring frame"))
			} else {
				Err(CaptureError::RefreshFailure)
			}
		}
	}

	pub fn get_output_data(&mut self) -> Result<(Vec<BGRA8>, (usize, usize)), CaptureError> {
		let mut frame_surface = match self.get_frame() {
			Ok(surface) => surface,
			Err(e) => return Err(e)
		};

		let mut mapped_surface = unsafe { zeroed() };
		if hr_failed(frame_surface.Map(&mut mapped_surface, DXGI_MAP_READ)) {
			frame_surface.Release();
			return Err(CaptureError::Fail("Failed to map surface"));
		}

		let output_desc = self.duplicated_output.as_mut().unwrap().get_desc();
		let (output_width, output_height) = {
			let RECT{ left, top, right, bottom } = output_desc.DesktopCoordinates;
			((right - left) as usize, (bottom - top) as usize)
		};

		let map_pitch_n_pixels = mapped_surface.Pitch as usize / DXGI_PIXEL_SIZE as usize;
		let mut pixel_buf = Vec::with_capacity(output_width * output_height);

		let pixel_index: Box<Fn(usize, usize) -> usize> = match output_desc.Rotation {
			DXGI_MODE_ROTATION::IDENTITY | DXGI_MODE_ROTATION::UNSPECIFIED => Box::new(
				|row, col| row * map_pitch_n_pixels + col),
			DXGI_MODE_ROTATION::ROTATE90 => Box::new(
				|row, col| (output_width-1-col) * map_pitch_n_pixels + row),
			DXGI_MODE_ROTATION::ROTATE180 => Box::new(
				|row, col| (output_height-1-row) * map_pitch_n_pixels + (output_width-col-1)),
			DXGI_MODE_ROTATION::ROTATE270 => Box::new(
				|row, col| col * map_pitch_n_pixels + (output_height-row-1))
		};

		let mapped_pixels = unsafe {
			slice::from_raw_parts(transmute(mapped_surface.pBits),
				output_width * output_height * map_pitch_n_pixels)
		};

		for row in 0..output_height {
			for col in 0..output_width {
				pixel_buf.push(mapped_pixels[pixel_index(row, col)]);
			}
		}

		frame_surface.Unmap();

		Ok((pixel_buf, (output_width, output_height)))
	}
}

#[test]
fn test() {
	let mut manager = DXGIManager::new(Duration::milliseconds(200)).unwrap();
	for _ in 0..10 {
		match manager.get_output_data() {
			Ok(pixels) => {
				let len = pixels.len() as u64;
				let (r, g, b) = pixels.into_iter()
					.fold((0u64, 0u64, 0u64), |(r, g, b), p|
						(r+p.r as u64, g+p.g as u64, b+p.b as u64));
				println!("avg: {} {} {}", r/len, g/len, b/len)
			},
			Err(e) => println!("error: {:?}", e)
		}
	}
}