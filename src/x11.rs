// ============================================================================
// X11 State
// ============================================================================

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd;
use tokio::sync::Mutex;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, warn};

use x11rb::CURRENT_TIME;
use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as XFixesConnectionExt, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, Property, PropertyNotifyEvent,
    SELECTION_NOTIFY_EVENT, SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::{
    CLIPBOARD_ATOM, ClipboardContent, ClipboardType, IMAGE_MIME_TYPES, INCR_ATOM, MULTIPLE_ATOM,
    PREFERRED_MIME_TYPES, PRIMARY_ATOM, SyncEvent, TARGETS_ATOM, TEXT_MIME_TYPES, TEXT_PLAIN_ATOM,
    TEXT_PLAIN_UTF8_ATOM, UTF8_STRING_ATOM, decode_clipboard_content,
};

pub struct X11State {
    conn: x11rb::rust_connection::RustConnection,
    _screen_num: usize,
    atoms: HashMap<String, Atom>,
    pub window: Window,
    sync_tx: tokio_mpsc::UnboundedSender<SyncEvent>,
    clipboard_content: Arc<Mutex<Option<ClipboardContent>>>,
    primary_content: Arc<Mutex<Option<ClipboardContent>>>,
    set_clipboard_rx: std_mpsc::Receiver<(ClipboardContent, ClipboardType)>,
    wake_read: OwnedFd,
}

impl X11State {
    pub fn new(
        conn: x11rb::rust_connection::RustConnection,
        screen_num: usize,
        sync_tx: tokio_mpsc::UnboundedSender<SyncEvent>,
        set_clipboard_rx: std_mpsc::Receiver<(ClipboardContent, ClipboardType)>,
        wake_read: OwnedFd,
    ) -> Result<Self, String> {
        let screen = &conn.setup().roots[screen_num];
        let window = conn
            .generate_id()
            .map_err(|e| format!("Failed to generate window ID: {}", e))?;

        info!("[X11] Creating window: {}", window);

        // Create a window
        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            screen.root_visual,
            &CreateWindowAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| format!("Failed to create window: {}", e))?;
        conn.flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        // Initialize XFixes extension
        let xfixes_query = conn
            .xfixes_query_version(5, 0)
            .map_err(|e| format!("Failed to query XFixes version: {}", e))?;
        let xfixes_reply = xfixes_query
            .reply()
            .map_err(|e| format!("Failed to get XFixes version reply: {}", e))?;
        info!(
            "[X11] XFixes version: {}.{}",
            xfixes_reply.major_version, xfixes_reply.minor_version
        );

        // Intern atoms
        let mut atoms = HashMap::new();
        let mut atom_names = vec![
            CLIPBOARD_ATOM,
            PRIMARY_ATOM,
            TARGETS_ATOM,
            MULTIPLE_ATOM,
            INCR_ATOM,
            UTF8_STRING_ATOM,
            TEXT_PLAIN_UTF8_ATOM,
            TEXT_PLAIN_ATOM,
        ];
        atom_names.extend_from_slice(IMAGE_MIME_TYPES);

        for name in &atom_names {
            let atom = conn
                .intern_atom(false, name.as_bytes())
                .map_err(|e| format!("Failed to intern atom {}: {}", name, e))?;
            let reply = atom
                .reply()
                .map_err(|e| format!("Failed to get atom reply for {}: {}", name, e))?;
            atoms.insert(name.to_string(), reply.atom);
            debug!("[X11] Interned atom: {} = {}", name, reply.atom);
        }

        // Set up XFixes selection event mask for CLIPBOARD
        if let Some(clipboard_atom) = atoms.get(CLIPBOARD_ATOM) {
            conn.xfixes_select_selection_input(
                window,
                *clipboard_atom,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|e| format!("Failed to select XFixes clipboard input: {}", e))?;
            info!("[X11] XFixes selection monitoring enabled for CLIPBOARD");
        }

        // Set up XFixes selection event mask for PRIMARY
        conn.xfixes_select_selection_input(
            window,
            AtomEnum::PRIMARY.into(),
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| format!("Failed to select XFixes primary input: {}", e))?;
        info!("[X11] XFixes selection monitoring enabled for PRIMARY");

        conn.flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(Self {
            conn,
            _screen_num: screen_num,
            atoms,
            window,
            sync_tx,
            clipboard_content: Arc::new(Mutex::new(None)),
            primary_content: Arc::new(Mutex::new(None)),
            set_clipboard_rx,
            wake_read,
        })
    }

    pub fn get_atom(&self, name: &str) -> Option<Atom> {
        self.atoms.get(name).copied()
    }

    fn intern_temp_atom(&self, name: &str) -> Result<Atom, String> {
        self.conn
            .intern_atom(false, name.as_bytes())
            .map_err(|e| format!("Failed to intern atom {}: {}", name, e))?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|e| format!("Failed to get atom reply for {}: {}", name, e))
    }

    fn mime_type_for_atom(&self, atom: Atom) -> Option<&'static str> {
        PREFERRED_MIME_TYPES
            .iter()
            .copied()
            .find(|mime_type| self.get_atom(mime_type) == Some(atom))
    }

    fn target_atoms_for_content(&self, content: Option<&ClipboardContent>) -> Vec<Atom> {
        let mut atoms = Vec::new();

        match content {
            Some(ClipboardContent::Text(_)) => {
                for mime_type in TEXT_MIME_TYPES {
                    if let Some(atom) = self.get_atom(mime_type) {
                        atoms.push(atom);
                    }
                }
            }
            Some(ClipboardContent::Data { mime_type, .. }) => {
                if let Some(atom) = self.get_atom(mime_type) {
                    atoms.push(atom);
                }
            }
            Some(ClipboardContent::Empty) | None => {}
        }

        if let Some(targets) = self.get_atom(TARGETS_ATOM) {
            atoms.push(targets);
        }

        atoms
    }

    fn choose_supported_targets(&self, offered_targets: &[Atom]) -> Vec<(Atom, &'static str)> {
        let mut targets = Vec::new();

        for mime_type in PREFERRED_MIME_TYPES {
            if let Some(atom) = self.get_atom(mime_type)
                && (offered_targets.is_empty() || offered_targets.contains(&atom))
            {
                targets.push((atom, *mime_type));
            }
        }

        targets
    }

    fn wait_for_selection_notify(
        &self,
        target: Atom,
        property: Atom,
    ) -> Result<Option<SelectionNotifyEvent>, String> {
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(20));

            match self.conn.poll_for_event() {
                Ok(Some(Event::SelectionNotify(notify)))
                    if notify.target == target || notify.property == property =>
                {
                    return Ok(Some(notify));
                }
                Ok(Some(Event::SelectionRequest(event))) => {
                    self.handle_selection_request(event)?;
                }
                Ok(Some(Event::PropertyNotify(_))) => {}
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(e) => {
                    debug!("[X11] Poll error while waiting for selection notify: {}", e);
                }
            }
        }

        Ok(None)
    }

    fn request_selection_targets(&self, selection_atom: Atom) -> Result<Vec<Atom>, String> {
        let targets = self.get_atom(TARGETS_ATOM).unwrap();
        let property = self.intern_temp_atom("CLIP_TEMP_TARGETS")?;

        self.conn
            .convert_selection(self.window, selection_atom, targets, property, CURRENT_TIME)
            .map_err(|e| format!("Failed to convert TARGETS selection: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        let Some(notify) = self.wait_for_selection_notify(targets, property)? else {
            debug!("[X11] No TARGETS response from selection owner");
            return Ok(Vec::new());
        };

        if notify.property == AtomEnum::NONE.into() {
            debug!("[X11] Selection owner did not provide TARGETS");
            return Ok(Vec::new());
        }

        let prop = self
            .conn
            .get_property(false, self.window, notify.property, AtomEnum::ATOM, 0, 1024)
            .map_err(|e| format!("Failed to get TARGETS property: {}", e))?
            .reply()
            .map_err(|e| format!("Failed to get TARGETS property reply: {}", e))?;

        let targets = prop.value32().into_iter().flatten().collect::<Vec<_>>();

        self.conn
            .delete_property(self.window, notify.property)
            .map_err(|e| format!("Failed to delete TARGETS property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(targets)
    }

    fn read_incr_property(&self, property: Atom) -> Result<Option<(Atom, Vec<u8>)>, String> {
        debug!("[X11] Reading INCR property: {}", property);

        self.conn
            .delete_property(self.window, property)
            .map_err(|e| format!("Failed to delete INCR property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        let mut bytes = Vec::new();
        let mut data_type = AtomEnum::NONE.into();

        for _ in 0..500 {
            std::thread::sleep(Duration::from_millis(20));

            match self.conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(event)))
                    if event.atom == property && event.state == Property::NEW_VALUE =>
                {
                    let prop = self
                        .conn
                        .get_property::<u32, u32>(
                            true,
                            self.window,
                            property,
                            AtomEnum::ANY.into(),
                            0,
                            u32::MAX,
                        )
                        .map_err(|e| format!("Failed to get INCR chunk: {}", e))?
                        .reply()
                        .map_err(|e| format!("Failed to get INCR chunk reply: {}", e))?;

                    if prop.value.is_empty() {
                        debug!("[X11] Finished INCR transfer: {} bytes", bytes.len());
                        return if bytes.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some((data_type, bytes)))
                        };
                    }

                    if data_type == AtomEnum::NONE.into() {
                        data_type = prop.type_;
                    }
                    bytes.extend_from_slice(&prop.value);
                }
                Ok(Some(Event::PropertyNotify(_))) => {}
                Ok(Some(Event::SelectionRequest(event))) => {
                    self.handle_selection_request(event)?;
                }
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(e) => {
                    debug!("[X11] Poll error while reading INCR property: {}", e);
                }
            }
        }

        warn!("[X11] INCR transfer timed out after {} bytes", bytes.len());
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some((data_type, bytes)))
        }
    }

    fn read_selection_property(&self, property: Atom) -> Result<Option<(Atom, Vec<u8>)>, String> {
        let prop = self
            .conn
            .get_property::<u32, u32>(
                false,
                self.window,
                property,
                AtomEnum::ANY.into(),
                0,
                u32::MAX,
            )
            .map_err(|e| format!("Failed to get property: {}", e))?
            .reply()
            .map_err(|e| format!("Failed to get property reply: {}", e))?;

        debug!(
            "[X11] Property read: type={}, format={}, bytes={}",
            prop.type_,
            prop.format,
            prop.value.len()
        );

        if prop.type_ == self.get_atom(INCR_ATOM).unwrap() {
            return self.read_incr_property(property);
        }

        if prop.type_ == 0 || prop.value.is_empty() {
            warn!("[X11] Property is empty or invalid");
            self.conn
                .delete_property(self.window, property)
                .map_err(|e| format!("Failed to delete property: {}", e))?;
            self.conn
                .flush()
                .map_err(|e| format!("Failed to flush connection: {}", e))?;
            return Ok(None);
        }

        let data = Some((prop.type_, prop.value));

        self.conn
            .delete_property(self.window, property)
            .map_err(|e| format!("Failed to delete property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(data)
    }

    fn request_selection_data(
        &self,
        selection_atom: Atom,
        target: Atom,
        property: Atom,
    ) -> Result<Option<(Atom, Vec<u8>)>, String> {
        self.conn
            .convert_selection(self.window, selection_atom, target, property, CURRENT_TIME)
            .map_err(|e| format!("Failed to convert selection: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        let Some(notify) = self.wait_for_selection_notify(target, property)? else {
            return Ok(None);
        };

        if notify.property == AtomEnum::NONE.into() {
            debug!(
                "[X11] Selection notify with NONE property for target {}",
                target
            );
            return Ok(None);
        }

        self.read_selection_property(notify.property)
    }

    fn decode_selection_content(
        &self,
        target: Atom,
        property_type: Atom,
        bytes: Vec<u8>,
    ) -> Result<Option<ClipboardContent>, String> {
        let mime_type = self
            .mime_type_for_atom(property_type)
            .or_else(|| self.mime_type_for_atom(target));

        let Some(mime_type) = mime_type else {
            warn!(
                "[X11] Unsupported property type: {} for target {}",
                property_type, target
            );
            return Ok(None);
        };

        decode_clipboard_content(mime_type, bytes).map(Some)
    }

    fn write_content_for_target(
        &self,
        requestor: Window,
        property: Atom,
        target: Atom,
        content: &ClipboardContent,
    ) -> Result<bool, String> {
        let targets = self.get_atom(TARGETS_ATOM).unwrap();

        if target == targets {
            let target_atoms = self.target_atoms_for_content(Some(content));
            self.conn
                .change_property32(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &target_atoms,
                )
                .map_err(|e| format!("Failed to change TARGETS property: {}", e))?;
            return Ok(true);
        }

        let Some(mime_type) = self.mime_type_for_atom(target) else {
            return Ok(false);
        };

        let Some(bytes) = content.bytes_for_mime(mime_type) else {
            return Ok(false);
        };

        self.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                requestor,
                property,
                target,
                bytes,
            )
            .map_err(|e| format!("Failed to change selection property: {}", e))?;

        Ok(true)
    }

    pub fn set_clipboard_content(
        &self,
        content: ClipboardContent,
        clipboard_type: ClipboardType,
    ) -> Result<(), String> {
        info!(
            "[X11] Setting clipboard content: type={:?}, len={}",
            clipboard_type,
            content.len()
        );

        let selection_atom = match clipboard_type {
            ClipboardType::Clipboard => self.get_atom(CLIPBOARD_ATOM).unwrap(),
            ClipboardType::Primary => AtomEnum::PRIMARY.into(),
        };

        // Store content
        let mime_type = content
            .primary_mime_type()
            .ok_or_else(|| "Cannot set empty clipboard content".to_string())?;
        let content_atom = self
            .get_atom(mime_type)
            .ok_or_else(|| format!("MIME atom not interned: {}", mime_type))?;
        let content_bytes = content
            .bytes_for_mime(mime_type)
            .ok_or_else(|| format!("Content cannot be represented as {}", mime_type))?;

        // Set property on our window
        self.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                self.window,
                content_atom,
                content_atom,
                content_bytes,
            )
            .map_err(|e| format!("Failed to change property: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        // Claim selection ownership
        self.conn
            .set_selection_owner(self.window, selection_atom, CURRENT_TIME)
            .map_err(|e| format!("Failed to set selection owner: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = Some(content);
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = Some(content);
            }
        }

        info!("[X11] Clipboard content set successfully");
        Ok(())
    }

    pub fn request_clipboard_content(&self, clipboard_type: ClipboardType) -> Result<(), String> {
        debug!("[X11] Requesting clipboard content: {:?}", clipboard_type);

        let selection_atom = match clipboard_type {
            ClipboardType::Clipboard => self.get_atom(CLIPBOARD_ATOM).unwrap(),
            ClipboardType::Primary => AtomEnum::PRIMARY.into(),
        };

        let owner = self
            .conn
            .get_selection_owner(selection_atom)
            .map_err(|e| format!("Failed to get selection owner: {}", e))?
            .reply()
            .map_err(|e| format!("Failed to get selection owner reply: {}", e))?;

        if owner.owner == self.window {
            debug!("[X11] We own the selection, using cached content");
            // We own it, use our cached content
            return Ok(());
        }

        if owner.owner == 0 {
            debug!("[X11] No selection owner");
            return Ok(());
        }

        debug!("[X11] Requesting selection from owner: {}", owner.owner);

        let offered_targets = self.request_selection_targets(selection_atom)?;
        let targets = self.choose_supported_targets(&offered_targets);

        for (index, (target, mime_type)) in targets.iter().enumerate() {
            let property = self.intern_temp_atom(&format!("CLIP_TEMP_{}", index))?;
            debug!(
                "[X11] Trying target {} ({}) with property {}",
                target, mime_type, property
            );

            let Some((property_type, bytes)) =
                self.request_selection_data(selection_atom, *target, property)?
            else {
                debug!("[X11] No valid response for target {}", target);
                continue;
            };

            let Some(content) = self.decode_selection_content(*target, property_type, bytes)?
            else {
                continue;
            };

            if content.is_empty() {
                debug!("[X11] Ignoring empty clipboard content");
                continue;
            }

            info!(
                "[X11] Received clipboard content: type={:?}, mime={:?}, len={}",
                clipboard_type,
                content.primary_mime_type(),
                content.len()
            );

            match clipboard_type {
                ClipboardType::Clipboard => {
                    *self.clipboard_content.blocking_lock() = Some(content.clone());
                }
                ClipboardType::Primary => {
                    *self.primary_content.blocking_lock() = Some(content.clone());
                }
            }

            match self.sync_tx.send(SyncEvent::X11ToWayland {
                content,
                clipboard_type,
            }) {
                Ok(_) => debug!("[X11] Sync event sent successfully"),
                Err(e) => error!("[X11] Failed to send sync event: {}", e),
            }

            return Ok(());
        }

        Ok(())
    }

    pub fn handle_selection_request(&self, event: SelectionRequestEvent) -> Result<(), String> {
        debug!("[X11] Selection request: {:?}", event);

        let targets = self.get_atom(TARGETS_ATOM).unwrap();
        let multiple = self.get_atom(MULTIPLE_ATOM).unwrap();

        let target = event.target;
        let mut property = if event.property == AtomEnum::NONE.into() {
            event.target
        } else {
            event.property
        };

        let content = match event.selection {
            s if s == self.get_atom(CLIPBOARD_ATOM).unwrap() => {
                self.clipboard_content.blocking_lock().clone()
            }
            s if s == AtomEnum::PRIMARY.into() => self.primary_content.blocking_lock().clone(),
            _ => None,
        };

        // Handle TARGETS request
        if target == targets {
            debug!("[X11] Handling TARGETS request");
            let target_atoms = self.target_atoms_for_content(content.as_ref());
            self.conn
                .change_property32(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &target_atoms,
                )
                .map_err(|e| format!("Failed to change property32: {}", e))?;
        }
        // Handle MULTIPLE request
        else if target == multiple {
            debug!("[X11] Handling MULTIPLE request");
            // Read the property and handle each atom pair
            let prop = self
                .conn
                .get_property(false, event.requestor, property, AtomEnum::ATOM, 0, 1024)
                .map_err(|e| format!("Failed to get property: {}", e))?
                .reply()
                .map_err(|e| format!("Failed to get property reply: {}", e))?;

            let mut atoms = prop.value32().into_iter().flatten().collect::<Vec<_>>();
            for chunk in atoms.chunks_mut(2) {
                if chunk.len() == 2 {
                    let request_target = chunk[0];
                    let request_property = chunk[1];
                    let handled = if request_property == AtomEnum::NONE.into() {
                        false
                    } else if request_target == targets {
                        let target_atoms = self.target_atoms_for_content(content.as_ref());
                        self.conn
                            .change_property32(
                                x11rb::protocol::xproto::PropMode::REPLACE,
                                event.requestor,
                                request_property,
                                AtomEnum::ATOM,
                                &target_atoms,
                            )
                            .map_err(|e| format!("Failed to change TARGETS property: {}", e))?;
                        true
                    } else if let Some(content) = content.as_ref() {
                        self.write_content_for_target(
                            event.requestor,
                            request_property,
                            request_target,
                            content,
                        )?
                    } else {
                        false
                    };

                    if !handled {
                        chunk[1] = AtomEnum::NONE.into();
                    }
                }
            }

            self.conn
                .change_property32(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &atoms,
                )
                .map_err(|e| format!("Failed to update MULTIPLE property: {}", e))?;
        }
        // Handle concrete data requests
        else if let Some(content) = content.as_ref() {
            debug!("[X11] Handling content request for target: {}", target);
            if !self.write_content_for_target(event.requestor, property, target, content)? {
                debug!("[X11] Unsupported target: {}", target);
                property = AtomEnum::NONE.into();
            }
        } else {
            warn!("[X11] No content available for request");
            property = AtomEnum::NONE.into();
        }

        // Send notification
        self.conn
            .send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property,
                },
            )
            .map_err(|e| format!("Failed to send event: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| format!("Failed to flush connection: {}", e))?;

        Ok(())
    }

    pub fn handle_selection_notify(&self, event: SelectionNotifyEvent) -> Result<(), String> {
        debug!("[X11] Selection notify: {:?}", event);

        if event.property == AtomEnum::NONE.into() {
            // Selection request failed
            warn!("[X11] Selection request failed (property is NONE)");
            return Ok(());
        }

        let Some((property_type, bytes)) = self.read_selection_property(event.property)? else {
            return Ok(());
        };

        let Some(content) = self.decode_selection_content(event.target, property_type, bytes)?
        else {
            return Ok(());
        };

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        info!(
            "[X11] Received clipboard content: type={:?}, mime={:?}, len={}",
            clipboard_type,
            content.primary_mime_type(),
            content.len()
        );

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = Some(content.clone());
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = Some(content.clone());
            }
        }

        // Send sync event
        let _ = self.sync_tx.send(SyncEvent::X11ToWayland {
            content,
            clipboard_type,
        });

        Ok(())
    }

    pub fn handle_selection_clear(&self, event: SelectionClearEvent) -> Result<(), String> {
        debug!("[X11] Selection clear: {:?}", event);

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        info!("[X11] Lost ownership of selection: {:?}", clipboard_type);

        match clipboard_type {
            ClipboardType::Clipboard => {
                *self.clipboard_content.blocking_lock() = None;
            }
            ClipboardType::Primary => {
                *self.primary_content.blocking_lock() = None;
            }
        }

        Ok(())
    }

    pub fn handle_property_notify(&self, event: PropertyNotifyEvent) -> Result<(), String> {
        debug!(
            "[X11] Property notify: atom={}, state={:?}",
            event.atom, event.state
        );
        Ok(())
    }

    fn handle_event(&self, event: Event) -> Result<(), String> {
        match event {
            Event::SelectionRequest(e) => self.handle_selection_request(e)?,
            Event::SelectionNotify(e) => self.handle_selection_notify(e)?,
            Event::SelectionClear(e) => self.handle_selection_clear(e)?,
            Event::PropertyNotify(e) => self.handle_property_notify(e)?,
            Event::XfixesSelectionNotify(e) => self.handle_xfixes_selection_notify(e)?,
            _ => {
                debug!("[X11] Unhandled event: {:?}", event);
            }
        }

        Ok(())
    }

    fn drain_wake_pipe(&self) {
        let mut buffer = [0u8; 64];
        match unistd::read(&self.wake_read, &mut buffer) {
            Ok(bytes) => debug!("[X11] Drained {} wake bytes", bytes),
            Err(e) => debug!("[X11] Failed to drain wake pipe: {}", e),
        }
    }

    fn drain_set_clipboard_requests(&self) -> Result<bool, String> {
        let mut did_work = false;

        loop {
            match self.set_clipboard_rx.try_recv() {
                Ok((content, clipboard_type)) => {
                    self.set_clipboard_content(content, clipboard_type)?;
                    did_work = true;
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    warn!("[X11] Set clipboard channel disconnected");
                    break;
                }
            }
        }

        Ok(did_work)
    }

    fn drain_x11_events(&self) -> Result<bool, String> {
        let mut did_work = false;

        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => {
                    self.handle_event(event)?;
                    did_work = true;
                }
                Ok(None) => break,
                Err(e) => {
                    debug!("[X11] Poll error: {}", e);
                    break;
                }
            }
        }

        Ok(did_work)
    }

    pub fn run_event_loop(&mut self) -> Result<(), String> {
        info!("[X11] Starting event loop");

        loop {
            let handled_commands = self.drain_set_clipboard_requests()?;
            let handled_events = self.drain_x11_events()?;

            if handled_commands || handled_events {
                let _ = self.conn.flush();
                continue;
            }

            let _ = self.conn.flush();

            let (x11_ready, wake_ready) = {
                let mut poll_fds = [
                    PollFd::new(self.conn.stream().as_fd(), PollFlags::POLLIN),
                    PollFd::new(self.wake_read.as_fd(), PollFlags::POLLIN),
                ];

                poll(&mut poll_fds, PollTimeout::NONE)
                    .map_err(|e| format!("Failed to poll X11 event fds: {}", e))?;

                let x11_ready = poll_fds[0]
                    .revents()
                    .unwrap_or_else(PollFlags::empty)
                    .intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP);
                let wake_ready = poll_fds[1]
                    .revents()
                    .unwrap_or_else(PollFlags::empty)
                    .intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP);

                (x11_ready, wake_ready)
            };

            if wake_ready {
                self.drain_wake_pipe();
            }

            if !x11_ready && !wake_ready {
                debug!("[X11] Poll returned without readable fds");
            }
        }
    }

    fn handle_xfixes_selection_notify(
        &self,
        event: x11rb::protocol::xfixes::SelectionNotifyEvent,
    ) -> Result<(), String> {
        debug!("[X11] XFixes selection notify: {:?}", event);

        let clipboard_type = if event.selection == self.get_atom(CLIPBOARD_ATOM).unwrap() {
            ClipboardType::Clipboard
        } else {
            ClipboardType::Primary
        };

        // Check if we own the selection
        if event.owner == self.window {
            debug!("[X11] We own the selection, ignoring");
            return Ok(());
        }

        // If there's a new owner (not none), request content
        if event.owner != 0 {
            info!(
                "[X11] Selection changed via XFixes: type={:?}, owner={}",
                clipboard_type, event.owner
            );
            let _ = self.request_clipboard_content(clipboard_type);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    #[test]
    fn test_x11_state_initialization() {
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let (sync_tx, _sync_rx) = unbounded_channel();
        let (_set_clipboard_tx, set_clipboard_rx) = mpsc::channel();
        let (wake_read, _wake_write) = unistd::pipe().unwrap();

        let x11_state = X11State::new(conn, screen_num, sync_tx, set_clipboard_rx, wake_read);
        assert!(x11_state.is_ok(), "Failed to initialize X11State");
    }

    #[test]
    fn test_atom_interning() {
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let (sync_tx, _sync_rx) = unbounded_channel();
        let (_set_clipboard_tx, set_clipboard_rx) = mpsc::channel();
        let (wake_read, _wake_write) = unistd::pipe().unwrap();

        let x11_state =
            X11State::new(conn, screen_num, sync_tx, set_clipboard_rx, wake_read).unwrap();

        // Test that all required atoms are interned
        let mut required_atoms = vec![
            CLIPBOARD_ATOM,
            PRIMARY_ATOM,
            TARGETS_ATOM,
            MULTIPLE_ATOM,
            INCR_ATOM,
            UTF8_STRING_ATOM,
            TEXT_PLAIN_UTF8_ATOM,
            TEXT_PLAIN_ATOM,
        ];
        required_atoms.extend_from_slice(IMAGE_MIME_TYPES);

        for atom_name in required_atoms {
            assert!(
                x11_state.get_atom(atom_name).is_some(),
                "Atom {} not interned",
                atom_name
            );
        }
    }
}
