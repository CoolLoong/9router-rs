use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use dispatch2::DispatchQueue;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Sel};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    NSWorkspace,
};
use objc2_foundation::{NSData, NSObject, NSPoint, NSSize, NSString, NSURL};
use tokio::sync::Notify;

#[path = "cli_tray_mac_startup.rs"]
mod startup;

struct ActionState {
    dashboard: Retained<NSURL>,
    shutdown: Arc<Notify>,
    startup: Option<startup::Startup>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ActionState]
    struct RouterTrayActions;

    impl RouterTrayActions {
        #[unsafe(method(openRouterDashboard:))]
        fn open_dashboard(&self, _sender: &AnyObject) {
            if !NSWorkspace::sharedWorkspace().openURL(&self.ivars().dashboard) {
                eprintln!("Could not open the 9Router dashboard");
            }
        }

        #[unsafe(method(quitRouter:))]
        fn quit(&self, _sender: &AnyObject) {
            println!("Shutting down from tray...");
            self.ivars().shutdown.notify_one();
        }

        #[unsafe(method(toggleRouterAutostart:))]
        fn toggle_autostart(&self, sender: &NSMenuItem) {
            let Some(startup) = &self.ivars().startup else { return };
            if let Err(error) = std::env::current_exe().and_then(|exe| startup.toggle(&exe)) {
                eprintln!("Could not update the macOS login startup: {error}");
            }
            update_autostart_menu(sender, Some(startup));
        }
    }
);

pub struct TraySession {
    app: Retained<NSApplication>,
    bar: Retained<NSStatusBar>,
    item: Retained<NSStatusItem>,
    actions: Retained<RouterTrayActions>,
    autostart: Retained<NSMenuItem>,
}

impl TraySession {
    pub fn enable_autostart(&self) -> io::Result<()> {
        let startup = self.actions.ivars().startup.as_ref();
        let result = if let Some(startup) = startup {
            std::env::current_exe().and_then(|exe| startup.enable(&exe))
        } else {
            Err(io::Error::other(
                "Could not locate the macOS home directory",
            ))
        };
        update_autostart_menu(&self.autostart, startup);
        result
    }

    pub fn run(&self) {
        autoreleasepool(|_| self.app.run());
        self.actions.ivars().shutdown.notify_one();
    }
}

impl Drop for TraySession {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            self.item.setMenu(None);
            self.bar.removeStatusItem(&self.item);
        });
    }
}

pub fn start(address: SocketAddr, shutdown: Arc<Notify>) -> Option<TraySession> {
    let dashboard = dashboard_url(address);
    let result = autoreleasepool(|_| {
        let main = MainThreadMarker::new()?;
        if objc2_core_graphics::CGMainDisplayID() == 0 {
            return None;
        }
        let app = NSApplication::sharedApplication(main);
        if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
            return None;
        }
        let url = NSURL::URLWithString(&NSString::from_str(&dashboard))?;
        let actions = RouterTrayActions::alloc(main).set_ivars(ActionState {
            dashboard: url,
            shutdown,
            startup: std::env::home_dir().map(|home| {
                startup::Startup::new(
                    &home,
                    unsafe { libc::getuid() },
                    Path::new("/bin/launchctl"),
                )
            }),
        });
        let actions: Retained<RouterTrayActions> = unsafe { msg_send![super(actions), init] };

        let menu = NSMenu::new(main);
        menu.setAutoenablesItems(false);
        let status = menu_item(main, &format!("9Router (Port {})", address.port()), None);
        status.setEnabled(false);
        menu.addItem(&status);
        let open = menu_item(main, "Open Dashboard", Some(sel!(openRouterDashboard:)));
        let autostart = menu_item(
            main,
            "Enable Auto-start",
            Some(sel!(toggleRouterAutostart:)),
        );
        let quit = menu_item(main, "Quit", Some(sel!(quitRouter:)));
        unsafe {
            open.setTarget(Some(&actions));
            autostart.setTarget(Some(&actions));
            quit.setTarget(Some(&actions));
        }
        menu.addItem(&open);
        update_autostart_menu(&autostart, actions.ivars().startup.as_ref());
        menu.addItem(&autostart);
        menu.addItem(&quit);

        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let Some(button) = item.button(main) else {
            bar.removeStatusItem(&item);
            return None;
        };
        let png = NSData::with_bytes(include_bytes!("../assets/tray/tray-icon.png"));
        if let Some(image) = NSImage::initWithData(NSImage::alloc(), &png) {
            image.setSize(NSSize::new(18.0, 18.0));
            button.setImage(Some(&image));
        } else {
            button.setTitle(&NSString::from_str("9Router"));
        }
        button.setToolTip(Some(&NSString::from_str(&format!(
            "9Router - Port {}",
            address.port()
        ))));
        item.setMenu(Some(&menu));
        Some(TraySession {
            app,
            bar,
            item,
            actions,
            autostart,
        })
    });
    if result.is_some() {
        println!("9Router is running in the macOS menu bar. Dashboard at {dashboard}");
    } else {
        println!("The macOS menu bar item is unavailable. The server is still running.");
        println!("Dashboard at {dashboard}");
    }
    result
}

fn update_autostart_menu(item: &NSMenuItem, startup: Option<&startup::Startup>) {
    let state = startup.and_then(|startup| startup.enabled().ok());
    let title = match state {
        Some(true) => "Disable Auto-start",
        Some(false) => "Enable Auto-start",
        None => "Auto-start unavailable",
    };
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(state.is_some());
}

fn menu_item(main: MainThreadMarker, label: &str, action: Option<Sel>) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(main),
            &NSString::from_str(label),
            action,
            &NSString::from_str(""),
        )
    }
}

pub struct StopOnDrop;

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        DispatchQueue::main().exec_async(|| {
            autoreleasepool(|_| {
                let Some(main) = MainThreadMarker::new() else {
                    return;
                };
                let app = NSApplication::sharedApplication(main);
                app.stop(None);
                if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                    NSEventType::ApplicationDefined, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(),
                    0.0, 0, None, 0, 0, 0,
                ) {
                    app.postEvent_atStart(&event, true);
                }
            });
        });
    }
}

fn dashboard_url(address: SocketAddr) -> String {
    if address.ip().is_unspecified() {
        if address.is_ipv6() {
            format!("http://[::1]:{}/dashboard", address.port())
        } else {
            format!("http://localhost:{}/dashboard", address.port())
        }
    } else {
        format!("http://{address}/dashboard")
    }
}
