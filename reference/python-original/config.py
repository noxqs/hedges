"""Read/write the single source of truth: ~/.config/fence/config.json.

Schema::

    {
      "fences": {
        "<name>": {
          "accent": "#7aa2f7", "bgColor": "#12141c", "bgOpacity": 0.55,
          "cornerRadius": 20, "columns": 4, "iconSize": 48,
          "tileSpacing": 12, "padding": 18,
          "showLabels": true, "showTitle": true, "showBackground": true,
          "expandOnHover": true,
          "x": 120, "y": 120, "edge": null,
          "apps": [ {"name": "Files", "command": "nemo", "icon": "system-file-manager"} ]
        }
      }
    }
"""
import json
import os

# Per-fence defaults. Geometry (x/y/edge) is filled in per instance.
DEFAULT_FENCE = {
    "accent": "#7aa2f7",
    "bgColor": "#12141c",
    "bgOpacity": 0.55,
    "cornerRadius": 20,
    "columns": 4,
    "iconSize": 48,
    "tileSpacing": 12,
    "padding": 18,
    "showLabels": True,
    "showTitle": True,
    "showBackground": True,
    "expandOnHover": True,
    "x": 120,
    "y": 120,
    "edge": None,
    "width": None,     # explicit size once the user resizes; None = auto-fit
    "height": None,
    "apps": [],
}

STARTER_APPS = [
    {"name": "Files",    "command": "nemo",              "icon": "system-file-manager"},
    {"name": "Terminal", "command": "gnome-terminal",    "icon": "utilities-terminal"},
    {"name": "Browser",  "command": "xdg-open https://", "icon": "web-browser"},
    {"name": "Settings", "command": "cinnamon-settings", "icon": "preferences-system"},
]


def config_dir():
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(
        os.path.expanduser("~"), ".config")
    return os.path.join(base, "fence")


def config_path():
    return os.path.join(config_dir(), "config.json")


def _fence_with_defaults(block):
    out = dict(DEFAULT_FENCE)
    out["apps"] = []
    if isinstance(block, dict):
        out.update(block)
    if not isinstance(out.get("apps"), list):
        out["apps"] = []
    return out


def default_config():
    fence = _fence_with_defaults({"apps": [dict(a) for a in STARTER_APPS]})
    return {"fences": {"Fence": fence}}


def load():
    """Load config, always returning a well-formed dict with normalised fences."""
    path = config_path()
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
    except (FileNotFoundError, ValueError, OSError):
        return default_config()

    fences = data.get("fences") if isinstance(data, dict) else None
    if not isinstance(fences, dict) or not fences:
        return default_config()

    return {"fences": {name: _fence_with_defaults(block)
                       for name, block in fences.items()}}


def save(cfg):
    """Atomically write the config to disk."""
    path = config_path()
    os.makedirs(config_dir(), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(cfg, fh, indent=4, ensure_ascii=False)
    os.replace(tmp, path)


# -- convenience operations on a loaded config -----------------------------

def fence_names(cfg):
    return list(cfg.get("fences", {}).keys())


def unique_name(cfg, base="Fence"):
    names = set(fence_names(cfg))
    if base not in names:
        return base
    i = 2
    while "%s %d" % (base, i) in names:
        i += 1
    return "%s %d" % (base, i)


def add_fence(cfg, name=None, seed_starter=False):
    name = name or unique_name(cfg)
    block = _fence_with_defaults(
        {"apps": [dict(a) for a in STARTER_APPS]} if seed_starter else {})
    cfg.setdefault("fences", {})[name] = block
    return name


def remove_fence(cfg, name):
    cfg.get("fences", {}).pop(name, None)


def rename_fence(cfg, old, new):
    fences = cfg.get("fences", {})
    if old not in fences or not new or new in fences:
        return False
    # preserve order
    cfg["fences"] = {(new if k == old else k): v for k, v in fences.items()}
    return True
