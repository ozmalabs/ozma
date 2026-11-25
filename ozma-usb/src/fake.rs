// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory fake gadget controller for tests and dry runs.
//!
//! The fake mirrors the observable ConfigFS contract (gadget presence, bind
//! state, function links, media attach/detach, typed missing-gadget errors)
//! without touching `/sys`, `/dev`, or any backing image. It is suitable for
//! node tests, controller dry runs, and API shape tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::config::{is_safe_identifier, UsbFunction, UsbFunctionKind, UsbGadgetConfig};
use crate::controller::UsbGadgetController;
use crate::error::UsbError;
use crate::functionfs::FunctionFsStatus;
use crate::media::{resolve_mass_storage_function, MediaState, VirtualMedia};
use crate::status::{FunctionStatus, GadgetStatus, UsbGadgetCapabilities};
use crate::uac2::Uac2Status;

/// An in-memory [`UsbGadgetController`].
#[derive(Debug)]
pub struct FakeUsbGadgetController {
    state: Mutex<FakeState>,
    capabilities: UsbGadgetCapabilities,
}

#[derive(Debug, Default)]
struct FakeState {
    gadgets: BTreeMap<String, FakeGadget>,
    binds: AtomicU64,
    unbinds: AtomicU64,
    fail_bind: AtomicBool,
}

#[derive(Debug, Clone)]
struct FakeGadget {
    config: UsbGadgetConfig,
    bound: Option<String>,
    media: BTreeMap<String, MediaState>,
}

impl Default for FakeUsbGadgetController {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeUsbGadgetController {
    /// A fake with all modelled gadget capabilities enabled.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeState::default()),
            capabilities: UsbGadgetCapabilities::available(),
        }
    }

    /// A fake that advertises no USB support.
    pub fn unavailable() -> Self {
        Self {
            state: Mutex::new(FakeState::default()),
            capabilities: UsbGadgetCapabilities::unavailable(),
        }
    }

    /// Use a custom capability advertisement.
    pub fn with_capabilities(capabilities: UsbGadgetCapabilities) -> Self {
        Self {
            state: Mutex::new(FakeState::default()),
            capabilities,
        }
    }

    /// Whether the named fake gadget is bound to a UDC.
    pub fn is_bound(&self, name: &str) -> bool {
        self.lock()
            .gadgets
            .get(name)
            .is_some_and(|gadget| gadget.bound.is_some())
    }

    /// Number of `bind` calls (including idempotent repeats).
    pub fn bind_calls(&self) -> u64 {
        self.lock().binds.load(Ordering::SeqCst)
    }

    /// Number of `unbind` calls (including idempotent repeats).
    pub fn unbind_calls(&self) -> u64 {
        self.lock().unbinds.load(Ordering::SeqCst)
    }

    /// Make every subsequent `bind` fail with a typed error.
    pub fn set_fail_bind(&self, fail: bool) {
        self.lock().fail_bind.store(fail, Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn status_of(gadget: &FakeGadget) -> GadgetStatus {
        let functions = gadget
            .config
            .functions
            .iter()
            .map(|function| {
                let name = function.configfs_name();
                let media = if function.kind() == UsbFunctionKind::MassStorage {
                    Some(
                        gadget
                            .media
                            .get(&name)
                            .cloned()
                            .unwrap_or(MediaState::Detached),
                    )
                } else {
                    None
                };
                let (uac2, function_fs) = match function {
                    UsbFunction::Uac2 { instance, config } => {
                        (Some(Uac2Status::from_config(instance, config)), None)
                    }
                    UsbFunction::Vendor { config } => {
                        (None, Some(FunctionFsStatus::from_config(config)))
                    }
                    _ => (None, None),
                };
                FunctionStatus {
                    name,
                    kind: function.kind(),
                    linked: true,
                    requires_userspace: function.requires_userspace(),
                    media,
                    uac2,
                    function_fs,
                }
            })
            .collect();
        GadgetStatus {
            name: gadget.config.name.clone(),
            present: true,
            bound: gadget.bound.is_some(),
            udc: gadget.bound.clone(),
            functions,
        }
    }
}

impl UsbGadgetController for FakeUsbGadgetController {
    fn create_gadget(&self, config: &UsbGadgetConfig) -> Result<GadgetStatus, UsbError> {
        config.validate()?;
        let mut state = self.lock();
        if state.gadgets.contains_key(&config.name) {
            return Err(UsbError::GadgetExists(config.name.clone()));
        }

        let mut media = BTreeMap::new();
        for function in &config.functions {
            if let crate::config::UsbFunction::MassStorage {
                instance,
                image: Some(image),
                ..
            } = function
            {
                // Match ConfigFS create: an initial image must exist.
                image.validate_for_attach()?;
                media.insert(format!("mass_storage.{instance}"), image.attached_state());
            }
        }

        let gadget = FakeGadget {
            config: config.clone(),
            bound: None,
            media,
        };
        let status = Self::status_of(&gadget);
        state.gadgets.insert(config.name.clone(), gadget);
        Ok(status)
    }

    fn remove_gadget(&self, name: &str) -> Result<(), UsbError> {
        let mut state = self.lock();
        if state.gadgets.remove(name).is_some() {
            Ok(())
        } else {
            Err(UsbError::MissingGadget(name.to_owned()))
        }
    }

    fn bind(&self, name: &str, udc: &str) -> Result<GadgetStatus, UsbError> {
        if !is_safe_identifier(name) {
            return Err(UsbError::InvalidField {
                field: "name",
                reason: format!("{name:?} is not a safe gadget name"),
            });
        }
        if udc.is_empty() || udc.contains('/') || udc.chars().any(char::is_whitespace) {
            return Err(UsbError::InvalidField {
                field: "udc",
                reason: format!("{udc:?} is not a valid UDC name"),
            });
        }
        let mut state = self.lock();
        state.binds.fetch_add(1, Ordering::SeqCst);
        if state.fail_bind.load(Ordering::SeqCst) {
            return Err(UsbError::InvalidField {
                field: "udc",
                reason: "fake controller was configured to reject the UDC bind".to_owned(),
            });
        }
        let gadget = state
            .gadgets
            .get_mut(name)
            .ok_or_else(|| UsbError::MissingGadget(name.to_owned()))?;
        if let Some(current) = &gadget.bound {
            if current == udc {
                return Ok(Self::status_of(gadget));
            }
            return Err(UsbError::AlreadyBound {
                gadget: name.to_owned(),
                udc: current.clone(),
            });
        }
        gadget.bound = Some(udc.to_owned());
        Ok(Self::status_of(gadget))
    }

    fn unbind(&self, name: &str) -> Result<GadgetStatus, UsbError> {
        let mut state = self.lock();
        state.unbinds.fetch_add(1, Ordering::SeqCst);
        let gadget = state
            .gadgets
            .get_mut(name)
            .ok_or_else(|| UsbError::MissingGadget(name.to_owned()))?;
        gadget.bound = None;
        Ok(Self::status_of(gadget))
    }

    fn status(&self, name: &str) -> Result<GadgetStatus, UsbError> {
        let state = self.lock();
        state
            .gadgets
            .get(name)
            .map(Self::status_of)
            .ok_or_else(|| UsbError::MissingGadget(name.to_owned()))
    }

    fn list_gadgets(&self) -> Result<Vec<String>, UsbError> {
        Ok(self.lock().gadgets.keys().cloned().collect())
    }

    fn attach_media(&self, gadget: &str, media: &VirtualMedia) -> Result<MediaState, UsbError> {
        media.validate()?;
        let function = resolve_mass_storage_function(&media.function)?;
        media.image.validate_for_attach()?;
        let state = media.image.attached_state();

        let mut state_lock = self.lock();
        let gadget_state = state_lock
            .gadgets
            .get_mut(gadget)
            .ok_or_else(|| UsbError::MissingGadget(gadget.to_owned()))?;
        if !gadget_state
            .config
            .functions
            .iter()
            .any(|entry| entry.configfs_name() == function)
        {
            return Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function,
            });
        }
        gadget_state.media.insert(function, state.clone());
        Ok(state)
    }

    fn detach_media(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError> {
        let function = resolve_mass_storage_function(function)?;
        let mut state = self.lock();
        let gadget_state = state
            .gadgets
            .get_mut(gadget)
            .ok_or_else(|| UsbError::MissingGadget(gadget.to_owned()))?;
        if !gadget_state
            .config
            .functions
            .iter()
            .any(|entry| entry.configfs_name() == function)
        {
            return Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function,
            });
        }
        gadget_state.media.insert(function, MediaState::Detached);
        Ok(MediaState::Detached)
    }

    fn media_state(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError> {
        let function = resolve_mass_storage_function(function)?;
        let state = self.lock();
        let gadget_state = state
            .gadgets
            .get(gadget)
            .ok_or_else(|| UsbError::MissingGadget(gadget.to_owned()))?;
        if !gadget_state
            .config
            .functions
            .iter()
            .any(|entry| entry.configfs_name() == function)
        {
            return Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function,
            });
        }
        Ok(gadget_state
            .media
            .get(&function)
            .cloned()
            .unwrap_or(MediaState::Detached))
    }

    fn capabilities(&self) -> UsbGadgetCapabilities {
        self.capabilities.clone()
    }
}
