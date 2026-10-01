#!/usr/bin/env python3
"""A private SNI watcher fixture: a bus owner alone is not a visible tray host."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from gi.repository import Gio, GLib

NAME = "org.kde.StatusNotifierWatcher"
XML = """<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
</interface></node>"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=("absent", "false", "true"), required=True)
    parser.add_argument("--exercise-menu", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or os.environ.get("MULTICORE_ISOLATED_SHELL_TEST") != "1":
        parser.error("Requires an isolated session and a test command")
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    registration = None
    items = []
    if args.host != "absent":
        properties = {
            "IsStatusNotifierHostRegistered": GLib.Variant("b", args.host == "true"),
            "RegisteredStatusNotifierItems": GLib.Variant("as", []),
            "ProtocolVersion": GLib.Variant("i", 0),
        }

        def method_call(_connection, sender, _path, _interface, method, parameters, invocation):
            if method == "RegisterStatusNotifierItem":
                service = parameters.unpack()[0]
                items.append((sender, service) if service.startswith("/") else (service, "/StatusNotifierItem"))
            invocation.return_value(GLib.Variant("()", ()))

        def get_property(_connection, _sender, _path, _interface, name):
            return properties[name]

        info = Gio.DBusNodeInfo.new_for_xml(XML)
        registration = connection.register_object("/StatusNotifierWatcher", info.interfaces[0],
                                                  method_call, get_property, None)
        result = connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                                      "org.freedesktop.DBus", "RequestName",
                                      GLib.Variant("(su)", (NAME, 4)), GLib.VariantType("(u)"),
                                      Gio.DBusCallFlags.NONE, 1000, None)
        if result.unpack() != (1,):
            raise RuntimeError("Private watcher name is already owned")
    temporary = tempfile.TemporaryDirectory(prefix="multicore-menu-", dir=os.environ["XDG_RUNTIME_DIR"])
    marker = Path(temporary.name) / "phase"
    environment = dict(os.environ, MULTICORE_TEST_HOST_PRESENT=str(args.host == "true").lower(),
                       MULTICORE_TEST_MENU_MARKER=str(marker))
    child = subprocess.Popen(command, env=environment)
    loop = GLib.MainLoop()
    sent = set()
    failures = []

    def call(service, path, interface, method, parameters=None):
        return connection.call_sync(service, path, interface, method, parameters, None,
                                    Gio.DBusCallFlags.NONE, 2000, None)

    def menu_event(label):
        service, item_path = items[-1]
        menu_path = call(service, item_path, "org.freedesktop.DBus.Properties", "Get",
                         GLib.Variant("(ss)", ("org.kde.StatusNotifierItem", "Menu"))).unpack()[0]
        layout = call(service, menu_path, "com.canonical.dbusmenu", "GetLayout",
                      GLib.Variant("(iias)", (0, -1, ["label", "enabled"]))).unpack()[1]

        def find(node):
            item_id, properties, children = node
            if properties.get("label", "").replace("_", "") == label:
                if not properties.get("enabled", True):
                    raise RuntimeError("Tray action unexpectedly disabled")
                return item_id
            for nested in children:
                found = find(nested)
                if found is not None:
                    return found
            return None

        item_id = find(layout)
        if item_id is None:
            raise RuntimeError(f"Missing tray action: {label}")
        call(service, menu_path, "com.canonical.dbusmenu", "Event",
             GLib.Variant("(isvu)", (item_id, "clicked", GLib.Variant("i", 0), 0)))

    def poll():
        if args.exercise_menu and child.poll() is None and marker.exists() and items:
            phase = marker.read_text()
            if phase and phase not in sent:
                try:
                    menu_event("Выйти" if phase == "exit" else "Открыть MULTICORE")
                    sent.add(phase)
                except Exception as error:
                    failures.append(str(error))
                    child.kill()
        if child.poll() is None:
            return True
        loop.quit()
        return False

    GLib.timeout_add(50, poll)
    try:
        loop.run()
        if failures:
            raise RuntimeError("; ".join(failures))
        if args.exercise_menu and child.returncode == 0:
            assert sent == {"restore-one", "restore-two", "exit"}, sent
        return child.returncode
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        if registration is not None:
            connection.unregister_object(registration)
        temporary.cleanup()


if __name__ == "__main__":
    sys.exit(main())
