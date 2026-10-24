"""Model and bake the spatial controls. Run with Blender's background Python.

    blender --background --python art/spatial-controls/build.py

The .blend and GLBs contain real, meter-scale geometry. Front orthographic PNGs
are the source for the compositor textures; oblique renders show the geometry.
Run pack.py with system Python (Pillow) after this script to pack runtime RGBA.
"""

import json
import math
from pathlib import Path

import bpy
from mathutils import Vector


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "renders"
OUTPUT.mkdir(parents=True, exist_ok=True)
bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene
scene.unit_settings.system = "METRIC"
scene.render.engine = "CYCLES"
scene.cycles.samples = 64
scene.cycles.use_denoising = True
scene.render.film_transparent = True
scene.render.image_settings.file_format = "PNG"
scene.render.image_settings.color_mode = "RGBA"
scene.render.image_settings.color_depth = "8"
scene.render.resolution_percentage = 100
scene.view_settings.view_transform = "AgX"
scene.world = bpy.data.worlds.new("Soft studio ambience")
scene.world.use_nodes = True
scene.world.node_tree.nodes["Background"].inputs["Color"].default_value = (0.55, 0.65, 0.8, 1)
scene.world.node_tree.nodes["Background"].inputs["Strength"].default_value = 0.35


def material(name, color, metallic=0.0, roughness=0.3):
    mat = bpy.data.materials.new(name)
    mat.diffuse_color = (*color, 1)
    mat.use_nodes = True
    shader = mat.node_tree.nodes.get("Principled BSDF")
    shader.inputs["Base Color"].default_value = (*color, 1)
    shader.inputs["Metallic"].default_value = metallic
    shader.inputs["Roughness"].default_value = roughness
    return mat


silver = material("Satin ceramic aluminium", (0.68, 0.77, 0.84), 0.55, 0.26)
graphite = material("Recessed graphite", (0.035, 0.055, 0.075), 0.25, 0.38)
pearl = material("Porcelain pointer", (0.92, 0.98, 1.0), 0.15, 0.22)
teal = material("Sea glass inlay", (0.025, 0.68, 0.58), 0.2, 0.24)


def finish(obj, name, mat):
    obj.name = name
    obj.data.materials.append(mat)
    for polygon in obj.data.polygons:
        polygon.use_smooth = True
    return obj


def rounded_prism(name, width, height, depth, radius, location, mat):
    outline = []
    for cx, cy, start in (
        (width / 2 - radius, height / 2 - radius, 0),
        (-width / 2 + radius, height / 2 - radius, 90),
        (-width / 2 + radius, -height / 2 + radius, 180),
        (width / 2 - radius, -height / 2 + radius, 270),
    ):
        for i in range(13):
            angle = math.radians(start + i * 90 / 12)
            outline.append((cx + radius * math.cos(angle), cy + radius * math.sin(angle)))
    count = len(outline)
    vertices = [(x, y, z) for z in (-depth / 2, depth / 2) for x, y in outline]
    faces = [tuple(reversed(range(count))), tuple(range(count, count * 2))]
    for i in range(count):
        j = (i + 1) % count
        faces.append((i, j, j + count, i + count))
    mesh = bpy.data.meshes.new(name)
    mesh.from_pydata(vertices, [], faces)
    mesh.update()
    obj = bpy.data.objects.new(name, mesh)
    scene.collection.objects.link(obj)
    obj.location = location
    bevel = obj.modifiers.new("Soft machined edge", "BEVEL")
    bevel.width = min(depth * 0.2, 0.0007)
    bevel.segments = 3
    obj.modifiers.new("Weighted face normals", "WEIGHTED_NORMAL")
    return finish(obj, name, mat)


def tube(name, points, radius, mat):
    curve = bpy.data.curves.new(name, "CURVE")
    curve.dimensions = "3D"
    curve.bevel_depth = radius
    curve.bevel_resolution = 4
    curve.use_fill_caps = True
    spline = curve.splines.new("POLY")
    spline.points.add(len(points) - 1)
    for point, xyz in zip(spline.points, points):
        point.co = (*xyz, 1)
    obj = bpy.data.objects.new(name, curve)
    scene.collection.objects.link(obj)
    bpy.context.view_layer.objects.active = obj
    obj.select_set(True)
    bpy.ops.object.convert(target="MESH")
    obj.select_set(False)
    return finish(obj, name, mat)


def torus(name, radius, thickness, z, mat):
    bpy.ops.mesh.primitive_torus_add(
        major_radius=radius, minor_radius=thickness,
        major_segments=64, minor_segments=12, location=(0, 0, z),
    )
    return finish(bpy.context.object, name, mat)


models = {}


def collect(name, builder):
    before = set(scene.objects)
    builder()
    objects = sorted(set(scene.objects) - before, key=lambda obj: obj.name)
    collection = bpy.data.collections.new(name)
    scene.collection.children.link(collection)
    for obj in objects:
        for old in list(obj.users_collection):
            old.objects.unlink(obj)
        collection.objects.link(obj)
    models[name] = objects


def grip():
    rounded_prism("Grip / silver shell", 0.224, 0.014, 0.0044, 0.0065, (0, 0, 0), silver)
    rounded_prism("Grip / inset", 0.212, 0.010, 0.0016, 0.0048, (0, 0, 0.0021), graphite)
    for i in range(-2, 3):
        rounded_prism(f"Grip / tactile rib {i + 3}", 0.0014, 0.0058, 0.0012, 0.00065,
                      (i * 0.005, 0, 0.0031), pearl)
    for side in (-1, 1):
        rounded_prism(f"Grip / teal inlay {side}", 0.019, 0.0016, 0.0008, 0.00075,
                      (side * 0.077, 0, 0.003), teal)


def corner():
    # Concentric with the runtime's rounded corner: radius .055 of screen width,
    # texture side .116. Blender +Y is up; packed image rows run downwards.
    scale = 0.07 / 0.116
    radius = 0.055 * scale
    def path(z):
        return [(radius * math.cos(a), radius * math.sin(a), z)
                for a in [3 * math.pi / 4 - 0.55 + i * 1.10 / 48 for i in range(49)]]
    for name, z, thickness, mat in (
        ("graphite underside", -0.0007, 0.003 * scale, graphite),
        ("satin edge", 0.0005, 0.0024 * scale, silver),
        ("inset guide", 0.0018, 0.00055 * scale, teal),
    ):
        points = path(z)
        tube(f"Corner / {name}", points, thickness, mat)
        for i, point in enumerate((points[0], points[-1])):
            bpy.ops.mesh.primitive_uv_sphere_add(segments=24, ring_count=12,
                                                radius=thickness, location=point)
            finish(bpy.context.object, f"Corner / {name} cap {i}", mat)


def pointer():
    torus("Pointer / contrast rim", 0.0085, 0.0016, -0.0005, graphite)
    torus("Pointer / pearl ring", 0.0085, 0.00105, 0.0007, pearl)
    points = [(0.0085 * math.cos(a), 0.0085 * math.sin(a), 0.0016)
              for a in [math.radians(55 + i * 70 / 24) for i in range(25)]]
    tube("Pointer / orientation inlay", points, 0.00045, teal)
    bpy.ops.mesh.primitive_uv_sphere_add(segments=32, ring_count=16, radius=0.0019)
    finish(bpy.context.object, "Pointer / precision dot", pearl)


collect("grip", grip)
collect("corner", corner)
collect("cursor", pointer)

bpy.ops.object.camera_add(location=(0, 0, 1))
camera = bpy.context.object
camera.name = "Orthographic bake / front +Z"
camera.data.type = "ORTHO"
camera.data.clip_start = 0.001
scene.camera = camera


def point_at(obj, target=(0, 0, 0)):
    obj.rotation_euler = (Vector(target) - obj.location).to_track_quat("-Z", "Y").to_euler()


for name, position, energy, size in (
    ("Large cool key", (-0.12, 0.18, 0.30), 2, 0.22),
    ("Soft fill", (0.16, -0.10, 0.22), 0.8, 0.18),
):
    bpy.ops.object.light_add(type="AREA", location=position)
    light = bpy.context.object
    light.name = name
    light.data.energy = energy
    light.data.shape = "DISK"
    light.data.size = size
    point_at(light)

specs = {
    "grip": {"pixels": [384, 32], "width_m": 0.24},
    "corner": {"pixels": [128, 128], "width_m": 0.07},
    "cursor": {"pixels": [96, 96], "width_m": 0.025},
}
manifest = {"blender": bpy.app.version_string, "assets": {}}
for name, objects in models.items():
    bpy.ops.object.select_all(action="DESELECT")
    for group, members in models.items():
        for obj in members:
            obj.hide_render = group != name
            obj.select_set(False)
    for obj in objects:
        obj.select_set(True)
    bpy.context.view_layer.objects.active = objects[0]
    bpy.ops.export_scene.gltf(filepath=str(ROOT / f"{name}.glb"), export_format="GLB",
                              use_selection=True, export_apply=True, export_yup=True)
    spec = specs[name]
    camera.location = (0, 0, 1)
    point_at(camera)
    camera.data.ortho_scale = spec["width_m"]
    scene.render.resolution_x, scene.render.resolution_y = spec["pixels"]
    scene.render.filepath = str(OUTPUT / f"{name}.png")
    bpy.ops.render.render(write_still=True)
    # Larger, oblique authoring view makes the actual depth inspectable.
    camera.location = (0.04, -0.12, 0.28)
    point_at(camera)
    camera.data.ortho_scale = spec["width_m"] * 1.1
    scene.render.resolution_x = 1200 if name == "grip" else 640
    scene.render.resolution_y = 300 if name == "grip" else 640
    scene.render.filepath = str(OUTPUT / f"{name}-model.png")
    bpy.ops.render.render(write_still=True)
    depsgraph = bpy.context.evaluated_depsgraph_get()
    triangles = 0
    for obj in objects:
        mesh = obj.evaluated_get(depsgraph).to_mesh()
        mesh.calc_loop_triangles()
        triangles += len(mesh.loop_triangles)
        obj.evaluated_get(depsgraph).to_mesh_clear()
    manifest["assets"][name] = {**spec, "triangles": triangles, "objects": [obj.name for obj in objects]}

# Keep the source scene convenient to inspect: collections laid out side by side.
for name, offset in (("grip", (0, 0.065, 0)), ("corner", (-0.06, -0.015, 0)),
                     ("cursor", (0.06, -0.015, 0))):
    for obj in models[name]:
        obj.hide_render = False
        obj.location += Vector(offset)
        obj.select_set(False)
camera.location = (0.02, -0.16, 0.38)
point_at(camera, (0, 0.03, 0))
camera.data.ortho_scale = 0.29
scene.render.resolution_x, scene.render.resolution_y = 1600, 1000
scene.render.filepath = str(OUTPUT / "models.png")
bpy.ops.render.render(write_still=True)
bpy.context.preferences.filepaths.save_version = 0
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT / "spatial-controls.blend"))
(ROOT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
