#!/usr/bin/env python
# run Minecraft Plus! as wayland dynamic wallpaper
# mirror https://plus.minecraft.net locally or use https://github.com/TheBrokenRail/MinecraftPlus
# it can also be modified to serve as a generic web-based wallpaper engine
# unfortunately, I can't use it as a screen locker (GtkSessionLock)
# since webkit can't handle gpu properly in that situation and will segfault then redscreen

from ctypes import CDLL
CDLL('libgtk4-layer-shell.so')

import gi, os, sys
gi.require_version("Gtk", "4.0")
gi.require_version('Gtk4LayerShell', '1.0')
gi.require_version("WebKit", "6.0")
from gi.repository import Gtk, Gtk4LayerShell, WebKit


#def on_load_changed(webview, event):
#    if event == WebKit.LoadEvent.FINISHED:
#        js = """
#            document.getElementById('webgl')?.click();
#            setTimeout(() => {
#              document.getElementById('window')?.click();
#            }, 100);
#        """
#        print("clicking button...")
#        webview.evaluate_javascript(js, -1, None, None)

def on_activate(app):
    window = Gtk.Window(application=app)

    webview = WebKit.WebView()
    settings = WebKit.Settings()
    settings.set_allow_file_access_from_file_urls(True)
    # settings.set_enable_developer_extras(True)
    webview.set_settings(settings)

    path = os.path.abspath(sys.argv[1])
    webview.load_uri("file://"+path)
    # webview.connect("load-changed", on_load_changed)

    window.set_child(webview)

    Gtk4LayerShell.init_for_window(window)
    Gtk4LayerShell.set_exclusive_zone(window, -1)
    Gtk4LayerShell.set_layer(window, Gtk4LayerShell.Layer.BACKGROUND)
    Gtk4LayerShell.set_anchor(window, Gtk4LayerShell.Edge.BOTTOM, 1)
    Gtk4LayerShell.set_anchor(window, Gtk4LayerShell.Edge.TOP, 1)
    Gtk4LayerShell.set_anchor(window, Gtk4LayerShell.Edge.LEFT, 1)
    Gtk4LayerShell.set_anchor(window, Gtk4LayerShell.Edge.RIGHT, 1)

    window.present()

if len(sys.argv) < 2:
    print(f"Usage: {__file__} <html_file>")
    quit()

app = Gtk.Application(application_id='com.github.wmww.gtk4-layer-shell.py-example')
app.connect('activate', on_activate)
app.run(None)
