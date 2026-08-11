#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""System-bus front end for the fixed-operation Wild Buzzard updater."""

from __future__ import annotations

import json
import os
import sys
import threading

from updater_core import (
    BusyError,
    PythonAptBackend,
    StalePlanError,
    UpdateEngine,
    UpdaterError,
    _sanitize_dynamic_text,
    validate_generation,
)


BUS_NAME = "org.openresearchtools.WildBuzzard.Updater1"
INTERFACE = BUS_NAME
OBJECT_PATH = "/org/openresearchtools/WildBuzzard/Updater1"
INTERACTIVE_UID = 1000
MAX_SENDER_UID_CACHE = 4_096

INTROSPECTION_XML = """
<node>
  <interface name="org.openresearchtools.WildBuzzard.Updater1">
    <method name="Check">
      <arg name="accepted" type="b" direction="out"/>
      <arg name="state_generation" type="t" direction="out"/>
    </method>
    <method name="GetState">
      <arg name="state_json" type="s" direction="out"/>
    </method>
    <method name="InstallPlan">
      <arg name="generation" type="s" direction="in"/>
      <arg name="accepted" type="b" direction="out"/>
      <arg name="state_generation" type="t" direction="out"/>
    </method>
    <method name="RetryRepair">
      <arg name="generation" type="s" direction="in"/>
      <arg name="accepted" type="b" direction="out"/>
      <arg name="state_generation" type="t" direction="out"/>
    </method>
    <method name="CancelDownload">
      <arg name="generation" type="s" direction="in"/>
      <arg name="accepted" type="b" direction="out"/>
      <arg name="state_generation" type="t" direction="out"/>
    </method>
  </interface>
</node>
"""


def _glib_modules():
    try:
        import gi

        gi.require_version("Gio", "2.0")
        from gi.repository import Gio, GLib
    except (ImportError, ValueError) as error:
        raise UpdaterError("python3-gi and Gio are required by the updater service") from error
    return Gio, GLib


class SystemBusService:
    def __init__(self, engine: UpdateEngine):
        self.engine = engine
        self.Gio, self.GLib = _glib_modules()
        self.loop = self.GLib.MainLoop()
        self.connection = None
        self.registration_id = None
        self.owner_id = None
        self.name_acquired = False
        self.fatal_bus_error: str | None = None
        self.uid_cache: dict[str, int] = {}
        self.uid_lock = threading.Lock()
        self.node = self.Gio.DBusNodeInfo.new_for_xml(INTROSPECTION_XML)
        self.interface = self.node.interfaces[0]

    def _sender_uid(self, connection, sender: str) -> int:
        with self.uid_lock:
            cached = self.uid_cache.get(sender)
        if cached is not None:
            return cached
        result = connection.call_sync(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixUser",
            self.GLib.Variant("(s)", (sender,)),
            self.GLib.VariantType("(u)"),
            self.Gio.DBusCallFlags.NONE,
            5_000,
            None,
        )
        uid = int(result.unpack()[0])
        with self.uid_lock:
            if len(self.uid_cache) >= MAX_SENDER_UID_CACHE:
                self.uid_cache.clear()
            self.uid_cache[sender] = uid
        return uid

    def _authorize(self, connection, sender: str) -> None:
        uid = self._sender_uid(connection, sender)
        if uid not in {0, INTERACTIVE_UID}:
            raise UpdaterError("only guest root and the interactive guest user may call the updater")

    def _return_error(self, invocation, error: BaseException) -> None:
        if isinstance(error, BusyError):
            name = f"{INTERFACE}.Error.Busy"
        elif isinstance(error, StalePlanError):
            name = f"{INTERFACE}.Error.StalePlan"
        elif isinstance(error, UpdaterError):
            name = f"{INTERFACE}.Error.Rejected"
        else:
            name = f"{INTERFACE}.Error.Failed"
        invocation.return_dbus_error(
            name,
            _sanitize_dynamic_text(error, fallback="updater request failed"),
        )

    def _method_call(
        self,
        connection,
        sender,
        _object_path,
        interface_name,
        method_name,
        parameters,
        invocation,
    ) -> None:
        try:
            if interface_name != INTERFACE:
                raise UpdaterError("unexpected updater interface")
            self._authorize(connection, sender)
            if method_name == "Check":
                if parameters.unpack() != ():
                    raise UpdaterError("Check takes no arguments")
                generation = self.engine.start_check()
                invocation.return_value(self.GLib.Variant("(bt)", (True, generation)))
            elif method_name == "GetState":
                if parameters.unpack() != ():
                    raise UpdaterError("GetState takes no arguments")
                invocation.return_value(self.GLib.Variant("(s)", (self.engine.state_json(),)))
            elif method_name in {"InstallPlan", "RetryRepair", "CancelDownload"}:
                unpacked = parameters.unpack()
                if not isinstance(unpacked, tuple) or len(unpacked) != 1:
                    raise UpdaterError(f"{method_name} takes exactly one opaque generation")
                generation = validate_generation(unpacked[0])
                if method_name == "InstallPlan":
                    state_generation = self.engine.start_install(generation)
                elif method_name == "RetryRepair":
                    state_generation = self.engine.start_repair(generation)
                else:
                    self.engine.cancel_download(generation)
                    state_generation = int(self.engine.state()["state_generation"])
                invocation.return_value(
                    self.GLib.Variant("(bt)", (True, state_generation))
                )
            else:
                raise UpdaterError("method is not part of the fixed updater interface")
        except BaseException as error:
            self._return_error(invocation, error)

    def _bus_acquired(self, connection, _name) -> None:
        try:
            self.connection = connection
            self.registration_id = connection.register_object(
                OBJECT_PATH,
                self.interface,
                self._method_call,
                None,
                None,
            )
        except BaseException as error:
            self.fatal_bus_error = _sanitize_dynamic_text(
                error,
                fallback="the updater could not register its fixed system-bus object",
            )
            self.loop.quit()

    def _name_acquired(self, _connection, _name) -> None:
        self.name_acquired = True

    def _name_lost(self, _connection, _name) -> None:
        self.fatal_bus_error = (
            "the updater lost its fixed system-bus endpoint"
            if self.name_acquired
            else "the updater could not own its fixed system-bus endpoint"
        )
        self.loop.quit()

    def run(self) -> int:
        if os.geteuid() != 0:
            raise UpdaterError("the system updater service must run as guest root")
        self.owner_id = self.Gio.bus_own_name(
            self.Gio.BusType.SYSTEM,
            BUS_NAME,
            self.Gio.BusNameOwnerFlags.NONE,
            self._bus_acquired,
            self._name_acquired,
            self._name_lost,
        )
        self.loop.run()
        if self.owner_id is not None:
            self.Gio.bus_unown_name(self.owner_id)
        raise UpdaterError(
            self.fatal_bus_error
            or "the updater system-bus main loop stopped unexpectedly"
        )


def _validate_introspection() -> None:
    Gio, _ = _glib_modules()
    node = Gio.DBusNodeInfo.new_for_xml(INTROSPECTION_XML)
    interface = node.interfaces[0]
    methods = {method.name for method in interface.methods}
    expected = {"Check", "GetState", "InstallPlan", "RetryRepair", "CancelDownload"}
    if methods != expected:
        raise UpdaterError("compiled updater D-Bus contract contains unexpected methods")


def main(arguments: list[str]) -> int:
    if arguments == ["--print-introspection"]:
        _validate_introspection()
        print(INTROSPECTION_XML.strip())
        return 0
    if arguments:
        raise UpdaterError("updater service accepts no command-line operation or apt arguments")
    _validate_introspection()
    return SystemBusService(UpdateEngine(PythonAptBackend())).run()


if __name__ == "__main__":
    try:
        exit_code = main(sys.argv[1:])
    except Exception as error:
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": _sanitize_dynamic_text(
                        error,
                        fallback="updater service failed during startup",
                    ),
                },
                sort_keys=True,
            ),
            file=sys.stderr,
        )
        raise SystemExit(1)
    raise SystemExit(exit_code)
