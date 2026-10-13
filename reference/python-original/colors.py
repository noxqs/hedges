"""Tiny colour helpers: normalise to an ``rgba()`` CSS string."""


def _parse(color):
    """Return (r, g, b) ints from '#rgb', '#rrggbb', or 'rgb(...)'/'rgba(...)'."""
    if not color:
        return (0, 0, 0)
    color = str(color).strip()
    if color.startswith("#"):
        h = color[1:]
        if len(h) == 3:
            h = "".join(c * 2 for c in h)
        try:
            return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))
        except ValueError:
            return (0, 0, 0)
    if color.startswith(("rgb(", "rgba(")):
        nums = color[color.find("(") + 1:color.rfind(")")].split(",")
        try:
            return tuple(int(float(nums[i])) for i in range(3))
        except (ValueError, IndexError):
            return (0, 0, 0)
    return (0, 0, 0)


def rgba(color, alpha=1.0):
    r, g, b = _parse(color)
    return "rgba(%d,%d,%d,%s)" % (r, g, b, _fmt(alpha))


def _fmt(a):
    s = ("%.3f" % float(a)).rstrip("0").rstrip(".")
    return s if s else "0"


def contrast_text(color):
    """A readable text colour (near-black or near-white) for this background."""
    r, g, b = _parse(color)
    lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0
    return "#12141c" if lum > 0.6 else "#f4f5f9"
