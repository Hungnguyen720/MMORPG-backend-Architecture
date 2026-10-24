#!/usr/bin/env python3
"""Generate the Ashfall asset pack sources: stand-in models (Godot text
scenes built from primitive meshes), ability and item icons, and the UI
theme. A real art team would replace these with Blender glTF exports at
the same paths; the engine only cares about the paths.

    python3 tenants/ashfall/assets/make_assets.py
    scripts/build-assets.sh ashfall      # zips assets/tenant/ into build/ashfall-assets.zip

Layout inside the pack (mounted at res:// by the client):
    tenant/models/<model tag>.tscn
    tenant/icons/<icon>.png
    tenant/theme.json
"""
import json
import math
import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "tenant")


# ---------------------------------------------------------------------------
# Models
# ---------------------------------------------------------------------------

class Scene:
    """Tiny writer for Godot 4 .tscn text scenes made of primitive meshes."""

    def __init__(self, name):
        self.name = name
        self.subs = []
        self.nodes = []

    def _sub(self, kind, props):
        sid = f"{kind}_{len(self.subs)}"
        self.subs.append((kind, sid, props))
        return sid

    def part(self, mesh, size, pos, color, tint=False, rot=(0, 0, 0), emission=None):
        """Add one mesh. `tint` parts take the mob colour from the data pack."""
        if mesh == "box":
            m = self._sub("BoxMesh", {"size": f"Vector3({size[0]}, {size[1]}, {size[2]})"})
        elif mesh == "capsule":
            m = self._sub("CapsuleMesh", {"radius": size[0], "height": size[1]})
        elif mesh == "sphere":
            m = self._sub("SphereMesh", {"radius": size[0], "height": size[1]})
        elif mesh == "cylinder":
            m = self._sub("CylinderMesh", {"top_radius": size[0], "bottom_radius": size[1], "height": size[2]})
        elif mesh == "prism":
            m = self._sub("PrismMesh", {"size": f"Vector3({size[0]}, {size[1]}, {size[2]})"})
        else:
            raise ValueError(mesh)
        mat = {"albedo_color": "Color(%.3f, %.3f, %.3f, 1)" % color}
        if emission:
            mat["emission_enabled"] = "true"
            mat["emission"] = "Color(%.3f, %.3f, %.3f, 1)" % emission
            mat["emission_energy_multiplier"] = 2.0
        mat_id = self._sub("StandardMaterial3D", mat)
        self.nodes.append((m, mat_id, pos, rot, tint))

    def write(self, path):
        lines = [f'[gd_scene load_steps={len(self.subs) + 1} format=3]', ""]
        for kind, sid, props in self.subs:
            lines.append(f'[sub_resource type="{kind}" id="{sid}"]')
            for k, v in props.items():
                lines.append(f"{k} = {v}")
            lines.append("")
        lines.append(f'[node name="{self.name}" type="Node3D"]')
        lines.append("")
        for i, (m, mat, pos, rot, tint) in enumerate(self.nodes):
            lines.append(f'[node name="Part{i}" type="MeshInstance3D" parent="."]')
            rx, ry, rz = (math.radians(a) for a in rot)
            # Basis from Euler XYZ (applied Z, then Y, then X is fine for our
            # single-axis rotations).
            cx, sx = math.cos(rx), math.sin(rx)
            cy, sy = math.cos(ry), math.sin(ry)
            cz, sz = math.cos(rz), math.sin(rz)
            b = [
                cy * cz, -cy * sz, sy,
                sx * sy * cz + cx * sz, -sx * sy * sz + cx * cz, -sx * cy,
                -cx * sy * cz + sx * sz, cx * sy * sz + sx * cz, cx * cy,
            ]
            # Godot's Transform3D text form lists basis columns.
            cols = [b[0], b[3], b[6], b[1], b[4], b[7], b[2], b[5], b[8]]
            vals = ", ".join("%.4f" % v for v in cols + list(pos))
            lines.append(f"transform = Transform3D({vals})")
            lines.append(f'mesh = SubResource("{m}")')
            lines.append(f'surface_material_override/0 = SubResource("{mat}")')
            if tint:
                lines.append('metadata/tint = true')
            lines.append("")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write("\n".join(lines))


BONE = (0.85, 0.82, 0.74)
ASH = (0.25, 0.22, 0.21)
EMBER = (1.0, 0.45, 0.1)
PALE = (0.92, 0.94, 1.0)
IRON = (0.32, 0.31, 0.33)
BLOOD = (0.55, 0.08, 0.1)


def warden():
    s = Scene("Warden")
    s.part("capsule", (0.42, 1.8), (0, 0.9, 0), (0.18, 0.16, 0.17))
    s.part("box", (1.0, 0.25, 0.55), (0, 1.5, 0), IRON)  # pauldrons
    s.part("sphere", (0.28, 0.5), (0, 1.95, 0), (0.12, 0.1, 0.1))
    s.part("prism", (0.5, 0.45, 0.5), (0, 2.3, 0), (0.12, 0.1, 0.1))  # hood point
    s.part("box", (0.08, 0.08, 0.08), (-0.09, 1.98, 0.24), EMBER, emission=EMBER)
    s.part("box", (0.08, 0.08, 0.08), (0.09, 1.98, 0.24), EMBER, emission=EMBER)
    s.part("box", (0.08, 1.6, 0.08), (0.55, 1.0, 0.2), BONE)  # scythe staff
    s.part("box", (0.7, 0.1, 0.06), (0.25, 1.8, 0.2), IRON, rot=(0, 0, -20))
    return s


def ghoul():
    s = Scene("Ghoul")
    s.part("capsule", (0.38, 1.4), (0, 0.8, 0.15), ASH, tint=True, rot=(25, 0, 0))
    s.part("sphere", (0.26, 0.45), (0, 1.35, 0.5), BONE)
    s.part("box", (0.12, 1.1, 0.12), (-0.45, 0.65, 0.35), ASH, tint=True, rot=(20, 0, 8))
    s.part("box", (0.12, 1.1, 0.12), (0.45, 0.65, 0.35), ASH, tint=True, rot=(20, 0, -8))
    s.part("box", (0.07, 0.07, 0.07), (-0.08, 1.38, 0.72), EMBER, emission=EMBER)
    s.part("box", (0.07, 0.07, 0.07), (0.08, 1.38, 0.72), EMBER, emission=EMBER)
    return s


def hound():
    s = Scene("Hound")
    s.part("box", (0.6, 0.55, 1.7), (0, 0.8, 0), ASH, tint=True)
    s.part("box", (0.45, 0.42, 0.6), (0, 1.0, 1.05), ASH, tint=True)
    s.part("box", (0.3, 0.1, 0.3), (0, 0.85, 1.4), EMBER, emission=EMBER)  # glowing maw
    for x in (-0.22, 0.22):
        for z in (-0.6, 0.6):
            s.part("box", (0.16, 0.55, 0.16), (x, 0.27, z), (0.12, 0.08, 0.06))
    for z in (-0.4, 0.0, 0.4):
        s.part("prism", (0.15, 0.35, 0.15), (0, 1.22, z), EMBER, emission=(0.8, 0.25, 0.05))
    return s


def acolyte():
    s = Scene("Acolyte")
    s.part("cylinder", (0.25, 0.55, 1.7), (0, 0.85, 0), PALE, tint=True)
    s.part("sphere", (0.25, 0.45), (0, 1.9, 0), (0.15, 0.13, 0.13))
    s.part("prism", (0.55, 0.7, 0.55), (0, 2.35, 0), PALE, tint=True)  # tall hood
    for a in range(5):
        x = math.cos(a * 1.256) * 0.35
        z = math.sin(a * 1.256) * 0.35
        s.part("cylinder", (0.04, 0.04, 0.25), (x, 2.05, z), PALE)
        s.part("sphere", (0.05, 0.1), (x, 2.22, z), PALE, emission=(0.8, 0.85, 1.0))
    return s


def colossus():
    s = Scene("Colossus")
    s.part("box", (1.1, 1.6, 0.8), (0, 2.0, 0), BONE, tint=True)
    for y in (1.5, 1.85, 2.2, 2.55):
        s.part("box", (1.3, 0.1, 0.9), (0, y, 0), BONE)  # ribs
    s.part("sphere", (0.45, 0.8), (0, 3.15, 0.1), BONE)
    s.part("box", (0.15, 0.15, 0.15), (-0.15, 3.2, 0.5), EMBER, emission=EMBER)
    s.part("box", (0.15, 0.15, 0.15), (0.15, 3.2, 0.5), EMBER, emission=EMBER)
    s.part("box", (0.35, 1.6, 0.35), (-0.9, 1.9, 0), BONE, rot=(0, 0, 10))
    s.part("box", (0.35, 1.6, 0.35), (0.9, 1.9, 0), BONE, rot=(0, 0, -10))
    s.part("box", (0.35, 1.2, 0.35), (-0.35, 0.6, 0), BONE)
    s.part("box", (0.35, 1.2, 0.35), (0.35, 0.6, 0), BONE)
    return s


def sentinel():
    s = Scene("Sentinel")
    s.part("capsule", (0.45, 1.9), (0, 0.95, 0), IRON, tint=True)
    s.part("box", (0.6, 0.5, 0.55), (0, 2.05, 0), IRON, tint=True)  # helm
    s.part("box", (0.4, 0.06, 0.05), (0, 2.08, 0.28), PALE, emission=(0.7, 0.8, 1.0))  # visor
    s.part("box", (0.9, 1.2, 0.12), (-0.55, 1.1, 0.35), IRON)  # tower shield
    s.part("box", (0.08, 1.3, 0.08), (0.55, 1.0, 0.3), (0.6, 0.6, 0.62))
    return s


def pale_king():
    s = Scene("PaleKing")
    s.part("capsule", (0.45, 2.0), (0, 1.0, 0), PALE, tint=True)
    s.part("box", (1.3, 1.9, 0.08), (0, 1.05, -0.35), BLOOD)  # cape
    s.part("sphere", (0.3, 0.55), (0, 2.15, 0), PALE)
    for a in range(7):
        x = math.cos(a * 0.9) * 0.3
        z = math.sin(a * 0.9) * 0.3
        s.part("prism", (0.12, 0.35, 0.12), (x, 2.55, z), (1.0, 0.82, 0.3), emission=(1.0, 0.6, 0.1))
    s.part("box", (0.1, 1.8, 0.1), (0.6, 1.1, 0.25), (0.75, 0.75, 0.8))
    return s


def effigy():
    s = Scene("Effigy")
    s.part("box", (0.3, 1.8, 0.3), (0, 0.9, 0), (0.15, 0.1, 0.08), tint=True)
    s.part("box", (1.6, 0.2, 0.2), (0, 1.4, 0), (0.15, 0.1, 0.08), tint=True)
    s.part("sphere", (0.3, 0.6), (0, 2.05, 0), (0.1, 0.07, 0.05))
    for x in (-0.6, 0.0, 0.6):
        s.part("sphere", (0.08, 0.16), (x, 1.55, 0.1), EMBER, emission=EMBER)
    return s


MODELS = {
    "warden": warden,
    "ghoul": ghoul,
    "quadruped_hound": hound,
    "acolyte": acolyte,
    "colossus": colossus,
    "sentinel": sentinel,
    "pale_king": pale_king,
    "dummy": effigy,
}


# ---------------------------------------------------------------------------
# Icons: 64x64 glyphs on a dark, ember-edged tile.
# ---------------------------------------------------------------------------

def tile(color):
    img = Image.new("RGBA", (64, 64), (18, 12, 12, 255))
    d = ImageDraw.Draw(img)
    d.rectangle([1, 1, 62, 62], outline=(90, 40, 20, 255), width=2)
    for i in range(20):
        a = int(70 * (1 - i / 20))
        d.ellipse([12 - i, 12 - i, 52 + i, 52 + i], outline=color[:3] + (a,))
    return img, d


def glyph(name, color, shape):
    img, d = tile(color)
    c = color + (255,) if len(color) == 3 else color
    if shape == "slash":
        d.line([12, 52, 52, 12], fill=c, width=7)
        d.line([18, 54, 54, 18], fill=(255, 220, 160, 255), width=2)
    elif shape == "bolt":
        d.polygon([(36, 6), (18, 36), (30, 36), (24, 58), (46, 26), (33, 26)], fill=c)
    elif shape == "drop":
        d.polygon([(32, 8), (16, 38), (48, 38)], fill=c)
        d.ellipse([16, 26, 48, 58], fill=c)
    elif shape == "skull":
        d.ellipse([16, 12, 48, 44], fill=c)
        d.rectangle([22, 40, 42, 52], fill=c)
        d.ellipse([22, 22, 30, 32], fill=(18, 12, 12, 255))
        d.ellipse([34, 22, 42, 32], fill=(18, 12, 12, 255))
    elif shape == "burst":
        for k in range(12):
            a = k * math.pi / 6
            d.line([32, 32, 32 + math.cos(a) * 26, 32 + math.sin(a) * 26], fill=c, width=4)
        d.ellipse([22, 22, 42, 42], fill=(255, 230, 170, 255))
    elif shape == "shield":
        d.polygon([(32, 8), (52, 16), (48, 42), (32, 56), (16, 42), (12, 16)], fill=c)
        d.line([32, 14, 32, 50], fill=(18, 12, 12, 255), width=3)
    elif shape == "flask":
        d.rectangle([28, 8, 36, 22], fill=(160, 150, 140, 255))
        d.ellipse([14, 20, 50, 56], fill=c)
    elif shape == "pile":
        for (x, y) in [(20, 40), (32, 34), (44, 42), (28, 46), (38, 48)]:
            d.ellipse([x - 8, y - 6, x + 8, y + 6], fill=c)
    elif shape == "fang":
        d.polygon([(22, 10), (42, 10), (32, 56)], fill=c)
    elif shape == "armor":
        d.rectangle([18, 16, 46, 54], fill=c)
        d.rectangle([10, 16, 22, 30], fill=c)
        d.rectangle([42, 16, 54, 30], fill=c)
        d.line([32, 18, 32, 52], fill=(18, 12, 12, 255), width=2)
    elif shape == "hood":
        d.polygon([(32, 8), (54, 52), (10, 52)], fill=c)
        d.ellipse([24, 30, 40, 46], fill=(18, 12, 12, 255))
    elif shape == "legs":
        d.rectangle([18, 10, 30, 56], fill=c)
        d.rectangle([34, 10, 46, 56], fill=c)
    elif shape == "blade":
        d.polygon([(30, 6), (36, 6), (36, 44), (30, 44)], fill=c)
        d.rectangle([20, 44, 46, 48], fill=(200, 160, 60, 255))
        d.rectangle([31, 48, 35, 58], fill=(120, 80, 40, 255))
    elif shape == "sickle":
        d.arc([10, 8, 54, 52], 180, 330, fill=c, width=6)
        d.line([32, 30, 32, 58], fill=(120, 80, 40, 255), width=4)
    elif shape == "heart":
        d.ellipse([12, 14, 34, 36], fill=c)
        d.ellipse([30, 14, 52, 36], fill=c)
        d.polygon([(13, 28), (51, 28), (32, 54)], fill=c)
    elif shape == "candle":
        d.rectangle([26, 26, 38, 56], fill=c)
        d.ellipse([28, 10, 36, 26], fill=(255, 190, 90, 255))
    return name, img


ICONS = [
    glyph("soul_rend", (255, 106, 43), "slash"),
    glyph("hex_bolt", (156, 77, 255), "bolt"),
    glyph("siphon", (210, 59, 92), "drop"),
    glyph("wither", (138, 154, 58), "skull"),
    glyph("ember_burst", (255, 176, 46), "burst"),
    glyph("grave_ward", (201, 195, 182), "shield"),
    glyph("tonic", (220, 60, 40), "flask"),
    glyph("salts", (110, 70, 230), "flask"),
    glyph("ash", (140, 135, 130), "pile"),
    glyph("fang", (230, 220, 200), "fang"),
    glyph("wax", (240, 236, 220), "candle"),
    glyph("cowl", (90, 80, 78), "hood"),
    glyph("plate", (150, 70, 40), "armor"),
    glyph("greaves", (110, 100, 96), "legs"),
    glyph("sickle", (190, 190, 200), "sickle"),
    glyph("heart", (230, 70, 60), "heart"),
    glyph("mail", (150, 150, 160), "armor"),
    glyph("blade", (240, 240, 255), "blade"),
]

THEME = {
    "accent": "#ff6a2b",
    "hp": "#b3261e",
    "mana": "#7a3cff",
    "xp": "#ff9a3d",
    "wall": "#3a2f2c",
    "portal": "#ff5a1f99",
    "sky": "#1a0f0d",
    "ambient": "#b07a60",
    "fog": "#2a1510",
}


def main():
    for tag, build in MODELS.items():
        build().write(os.path.join(OUT, "models", f"{tag}.tscn"))
    os.makedirs(os.path.join(OUT, "icons"), exist_ok=True)
    for name, img in ICONS:
        img.save(os.path.join(OUT, "icons", f"{name}.png"))
    with open(os.path.join(OUT, "theme.json"), "w") as f:
        json.dump(THEME, f, indent=2)
    print(f"wrote {len(MODELS)} models, {len(ICONS)} icons and theme.json to {OUT}")


if __name__ == "__main__":
    main()
