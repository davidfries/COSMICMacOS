// SPDX-License-Identifier: GPL-3.0-or-later
//! AppKit glue: the `NSStatusItem`, its `NSMenu`, the menu actions and the
//! hand-off from the background threads.
//!
//! Everything in this module runs on the main thread, inside
//! `-[NSApplication run]`:
//! - [`run`] sets the `Accessory` activation policy (no Dock icon, no app
//!   menu) and starts the run loop; the delegate's
//!   `applicationDidFinishLaunching:` creates the status item and starts the
//!   two background threads (`crate::connection`).
//! - The subscription thread hands each report to [`deliver`], which queues
//!   a block on the main dispatch queue.  AppKit drains that queue in the
//!   common run-loop modes, so updates also arrive while the menu is open.
//! - Menu items call `-[MenuTarget performCommand:]`, which only queues the
//!   request for the action thread (or quits).
//!
//! What the item and the menu show comes from [`model::menu`]; this module
//! copies a [`MenuModel`] into AppKit and nothing else.

use std::cell::RefCell;
use std::ops::ControlFlow;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;

use cosmic_mac_ctl::client::Client;
use cosmic_mac_protocol::Request;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSControlStateValueOff,
    NSControlStateValueOn, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, ns_string};

use crate::connection::{self, Backoff};
use crate::model::{self, Connection, MenuCommand, MenuEntry, MenuModel};

thread_local! {
    /// The status item.  Only the main thread ever sets or reads it.
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// The AppKit objects behind the status item.  Kept for the life of the
/// process: the menu items' targets are weak references to `target`.
struct Ui {
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<MenuTarget>,
    /// The model on screen, so an unchanged model is not re-applied.
    shown: Option<MenuModel>,
}

impl Ui {
    fn show(&mut self, model: MenuModel, mtm: MainThreadMarker) {
        if self.shown.as_ref() == Some(&model) {
            return;
        }
        if let Some(button) = self.item.button(mtm) {
            button.setTitle(&NSString::from_str(&model.title));
            button.setToolTip(Some(&NSString::from_str(&model.tooltip)));
        }
        // The menu is a dozen items at most: rebuilding it is simpler than
        // diffing, and AppKit redraws an open menu after the change.
        self.menu.removeAllItems();
        for entry in &model.entries {
            let item = match entry {
                MenuEntry::Separator => NSMenuItem::separatorItem(mtm),
                MenuEntry::Item(entry) => self.menu_item(entry, mtm),
            };
            self.menu.addItem(&item);
        }
        self.shown = Some(model);
    }

    fn menu_item(&self, entry: &model::MenuItem, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let action = entry.command.map(|_| sel!(performCommand:));
        // SAFETY: `performCommand:` is a valid selector, implemented by
        // `MenuTarget`, which is set as the target of every item that has it.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&entry.title),
                action,
                ns_string!(""),
            )
        };
        if let Some(command) = entry.command {
            let target: &AnyObject = &self.target;
            // SAFETY: the target implements the item's action, and `Ui`
            // keeps it alive for the life of the process (the property is weak).
            unsafe { item.setTarget(Some(target)) };
            item.setTag(command.tag());
        }
        item.setEnabled(entry.enabled);
        item.setState(if entry.checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        item
    }
}

/// Instance variables of [`MenuTarget`].
struct TargetIvars {
    /// Requests for the action thread.
    actions: mpsc::Sender<Request>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `MenuTarget` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CosmicMacStatusMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        /// The action of every clickable menu item; the item's tag names the
        /// command.
        // SAFETY: an action method takes the sender and returns nothing.
        #[unsafe(method(performCommand:))]
        fn perform_command(&self, sender: Option<&AnyObject>) {
            let Some(item) = sender.and_then(|sender| sender.downcast_ref::<NSMenuItem>()) else {
                return;
            };
            let Some(command) = MenuCommand::from_tag(item.tag()) else {
                log::warn!("menu item without a command (tag {})", item.tag());
                return;
            };
            match command.request() {
                Some(request) => {
                    if self.ivars().actions.send(request).is_err() {
                        log::error!("the action thread has stopped");
                    }
                }
                None => {
                    log::info!("quit from the menu");
                    NSApplication::sharedApplication(self.mtm()).terminate(None);
                }
            }
        }
    }

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, actions: mpsc::Sender<Request>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { actions });
        // SAFETY: `-[NSObject init]` has this signature.
        unsafe { msg_send![super(this), init] }
    }
}

/// Instance variables of [`AppDelegate`].
struct DelegateIvars {
    client: Client,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `AppDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CosmicMacStatusAppDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: `NSApplicationDelegate` has no safety requirements.
    unsafe impl NSApplicationDelegate for AppDelegate {
        // SAFETY: the signature is correct.
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            if let Err(error) = start(self.mtm(), self.ivars().client.clone()) {
                log::error!("{error}");
                // A non-zero exit, so launchd's KeepAlive restarts the agent.
                std::process::exit(1);
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, client: Client) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars { client });
        // SAFETY: `-[NSObject init]` has this signature.
        unsafe { msg_send![super(this), init] }
    }
}

/// Create the status item (showing the disconnected state until the first
/// report) and start the background threads.
fn start(mtm: MainThreadMarker, client: Client) -> Result<(), String> {
    let actions = connection::spawn_action_worker(client.clone())
        .map_err(|error| format!("cannot start the action thread: {error}"))?;

    let menu = NSMenu::new(mtm);
    // Items keep the enabled flag the model gives them.
    menu.setAutoenablesItems(false);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    item.setMenu(Some(&menu));
    let mut ui = Ui {
        item,
        menu,
        target: MenuTarget::new(mtm, actions),
        shown: None,
    };
    ui.show(model::menu(&Connection::Disconnected), mtm);
    UI.with_borrow_mut(|slot| *slot = Some(ui));

    log::info!(
        "status item ready; subscribing to {}",
        client.path().display()
    );
    thread::Builder::new()
        .name("cosmic-mac-status-subscription".into())
        .spawn(move || {
            connection::run_subscription(&client, Backoff::default(), |connection| {
                deliver(connection);
                ControlFlow::Continue(())
            });
        })
        .map_err(|error| format!("cannot start the subscription thread: {error}"))?;
    Ok(())
}

/// Show `connection` in the status item.  Callable from any thread: the
/// update runs on the main thread.
fn deliver(connection: Connection) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            log::error!("a main-queue block ran off the main thread");
            return;
        };
        let model = model::menu(&connection);
        UI.with_borrow_mut(|ui| {
            if let Some(ui) = ui {
                ui.show(model, mtm);
            }
        });
    });
}

/// Run the status item until it quits.  Must be called on the main thread.
pub fn run(client: Client) -> ExitCode {
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("cosmic-mac-status must run on the main thread");
        return ExitCode::FAILURE;
    };
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon and no app menu: the process only owns the status item.
    // Set before `run`, so the Dock never shows the process.
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        log::warn!("setActivationPolicy(Accessory) was refused");
    }
    let delegate = AppDelegate::new(mtm, client);
    // The delegate property is weak; `delegate` lives until `run` returns.
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    // `run` only returns after `stop:`; Quit (`terminate:`) exits the process.
    drop(delegate);
    ExitCode::SUCCESS
}
