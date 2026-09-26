#!/usr/bin/env python3
"""A private SNI watcher fixture: a bus owner alone is not a visible tray host."""
import argparse
import os
import subprocess
import sys

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
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or os.environ.get("MULTICORE_ISOLATED_SHELL_TEST") != "1":
        parser.error("Requires an isolated session and a test command")
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    registration = None
    if args.host != "absent":
        properties = {
            "IsStatusNotifierHostRegistered": GLib.Variant("b", args.host == "true"),
            "RegisteredStatusNotifierItems": GLib.Variant("as", []),
            "ProtocolVersion": GLib.Variant("i", 0),
        }

        def method_call(_connection, _sender, _path, _interface, _method, _parameters, invocation):
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
    environment = dict(os.environ, MULTICORE_TEST_HOST_PRESENT=str(args.host == "true").lower())
    child = subprocess.Popen(command, env=environment)
    loop = GLib.MainLoop()

    def poll():
        if child.poll() is None:
            return True
        loop.quit()
        return False

    GLib.timeout_add(50, poll)
    try:
        loop.run()
        return child.returncode
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        if registration is not None:
            connection.unregister_object(registration)


if __name__ == "__main__":
    sys.exit(main())
