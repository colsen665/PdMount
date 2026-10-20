#!/usr/bin/env python3
"""Stand-in for gcr-prompter that never shows or answers a prompt (D024).

It owns org.gnome.keyring.SystemPrompter before D-Bus activation can start the
real gcr-prompter. gnome-keyring's BeginPrompting call succeeds and the prompt
then never progresses. To the Secret Service client this looks the same as an
unlock dialog nobody answers: Prompt.Completed never arrives.

Usage: stub-prompter.py <log file>   (run inside the harness's private bus)
"""
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

INTERFACE = "org.gnome.keyring.internal.Prompter"
log = open(sys.argv[1], "a", buffering=1)


class Prompter(dbus.service.Object):
    @dbus.service.method(INTERFACE, in_signature="o", out_signature="")
    def BeginPrompting(self, callback):
        log.write(f"BeginPrompting {callback} (never answered)\n")

    @dbus.service.method(INTERFACE, in_signature="osa{sv}s", out_signature="")
    def PerformPrompt(self, callback, kind, properties, exchange):
        log.write(f"PerformPrompt {callback} {kind}\n")

    @dbus.service.method(INTERFACE, in_signature="o", out_signature="")
    def StopPrompting(self, callback):
        log.write(f"StopPrompting {callback}\n")


DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName("org.gnome.keyring.SystemPrompter", bus, do_not_queue=True)
Prompter(bus, "/org/gnome/keyring/Prompter")
log.write("stub prompter ready\n")
GLib.MainLoop().run()
