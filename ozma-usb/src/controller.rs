// SPDX-License-Identifier: AGPL-3.0-only
//! The gadget-controller trait shared by Linux ConfigFS, fake, and future
//! node implementations.

use crate::config::UsbGadgetConfig;
use crate::error::UsbError;
use crate::media::{MediaState, VirtualMedia};
use crate::status::{GadgetStatus, UsbGadgetCapabilities};

/// Create, bind, inspect, and manage composite USB gadgets and virtual media.
///
/// Implementations are synchronous and object safe so a node can hold a
/// `Box<dyn UsbGadgetController>` selected from configuration. They must not
/// mount filesystems, write block devices, or open a real HID endpoint; those
/// are separate node/media-layer concerns.
pub trait UsbGadgetController: Send + Sync {
    /// Create the gadget directory, descriptors, strings, functions, and links.
    ///
    /// The gadget is left **unbound**; call [`Self::bind`] afterwards, or use
    /// [`Self::create_and_bind`]. On failure, implementations should remove a
    /// partially created gadget when they can do so safely.
    fn create_gadget(&self, config: &UsbGadgetConfig) -> Result<GadgetStatus, UsbError>;

    /// Remove the gadget and its functions/links. Missing gadgets are a typed
    /// [`UsbError::MissingGadget`].
    fn remove_gadget(&self, name: &str) -> Result<(), UsbError>;

    /// Bind the gadget to a UDC. Idempotent when already bound to the same UDC.
    fn bind(&self, name: &str, udc: &str) -> Result<GadgetStatus, UsbError>;

    /// Unbind the gadget from its UDC. Idempotent when already unbound.
    fn unbind(&self, name: &str) -> Result<GadgetStatus, UsbError>;

    /// Read the current gadget state.
    fn status(&self, name: &str) -> Result<GadgetStatus, UsbError>;

    /// List gadget names under the configured root.
    fn list_gadgets(&self) -> Result<Vec<String>, UsbError>;

    /// Attach a backing image to a mass-storage function.
    ///
    /// The image model is validated and its file must exist. Read-only and
    /// CD-ROM flags are written before the `file` attribute, matching the
    /// kernel requirement that LUN flags change while the LUN is empty.
    fn attach_media(&self, gadget: &str, media: &VirtualMedia) -> Result<MediaState, UsbError>;

    /// Clear the `file` attribute for a mass-storage function, detaching its
    /// media. The function and gadget stay in place.
    fn detach_media(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError>;

    /// Read the current media state of a mass-storage function.
    fn media_state(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError>;

    /// Capabilities visible through this controller.
    fn capabilities(&self) -> UsbGadgetCapabilities;

    /// Create a gadget and bind it to its configured UDC.
    ///
    /// If binding fails, the partially created gadget is removed and the bind
    /// error is returned.
    fn create_and_bind(&self, config: &UsbGadgetConfig) -> Result<GadgetStatus, UsbError> {
        self.create_gadget(config)?;
        match self.bind(&config.name, &config.udc) {
            Ok(status) => Ok(status),
            Err(error) => {
                let _ = self.remove_gadget(&config.name);
                Err(error)
            }
        }
    }
}
