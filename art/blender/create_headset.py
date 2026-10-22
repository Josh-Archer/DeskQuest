"""Original Quest 3 approximation and neutral fallback; no downloaded geometry.

blender --background --python art/blender/create_headset.py -- --root /path/to/repo
The runtime mesh uses metres, X right, Y up and Z toward the headset's front.
"""
import argparse
import math
from pathlib import Path
import struct
import sys

import bpy
import bmesh
from mathutils import Vector

args = argparse.ArgumentParser()
args.add_argument('--root', type=Path, required=True)
root = args.parse_args(sys.argv[sys.argv.index('--') + 1:]).root.resolve()
assets = root / 'assets/headsets'
assets.mkdir(parents=True, exist_ok=True)


def material(name, color, roughness=.45, metallic=0):
    mat = bpy.data.materials.new(name)
    mat.diffuse_color = (*color, 1)
    mat.use_nodes = True
    shader = mat.node_tree.nodes.get('Principled BSDF')
    shader.inputs['Base Color'].default_value = (*color, 1)
    shader.inputs['Roughness'].default_value = roughness
    shader.inputs['Metallic'].default_value = metallic
    return mat


white = material('Warm white polymer', (.86, .88, .91), .30)
face_white = material('Front face polymer', (.96, .97, .98), .28)
black = material('Matte black sensor surrounds', (.012, .016, .024), .28)
glass = material('Optical glass', (.008, .018, .034), .22, .10)
fabric = material('Soft woven strap', (.70, .73, .77), .88)
seam = material('Strap edge stitching', (.47, .50, .54), .93)
pad = material('Charcoal facial cushion', (.035, .041, .049), .95)
metal = material('Lens retaining rings', (.11, .14, .18), .28, .6)
led = material('Status light', (.14, .66, .96), .18)


def finish(obj, name, mat):
    obj.name = name
    obj.data.materials.append(mat)
    bm = bmesh.new()
    bm.from_mesh(obj.data)
    bmesh.ops.recalc_face_normals(bm, faces=list(bm.faces))
    bm.to_mesh(obj.data)
    bm.free()
    for face in obj.data.polygons:
        face.use_smooth = True
    return obj


def box(name, center, dimensions, mat, bevel=.006):
    bpy.ops.mesh.primitive_cube_add(size=1, location=center)
    obj = bpy.context.object
    obj.dimensions = dimensions
    bpy.ops.object.transform_apply(location=False, rotation=False, scale=True)
    finish(obj, name, mat)
    modifier = obj.modifiers.new('Rounded moulded edges', 'BEVEL')
    modifier.width = min(bevel, min(dimensions) * .48)
    modifier.segments = 5
    normal = obj.modifiers.new('Weighted surface normals', 'WEIGHTED_NORMAL')
    normal.keep_sharp = True
    return obj


def lens(name, x, y, z, radius, mat):
    bpy.ops.mesh.primitive_cylinder_add(vertices=32, radius=radius, depth=.0015,
                                      location=(x, y, z), rotation=(math.pi / 2, 0, 0))
    return finish(bpy.context.object, name, mat)


def ribbon(name, path, width, thickness, mat, horizontal=False):
    vertices, faces = [], []
    for i, point in enumerate(path):
        tangent = Vector(path[min(i + 1, len(path) - 1)]) - Vector(path[max(i - 1, 0)])
        across = Vector((0, 0, 1)) if horizontal else Vector((1, 0, 0))
        normal = tangent.cross(across).normalized()
        for side, depth in ((-1, -1), (1, -1), (1, 1), (-1, 1)):
            vertices.append(Vector(point) + across * side * width / 2 + normal * depth * thickness / 2)
    for i in range(len(path) - 1):
        for j in range(4):
            a = i * 4 + j
            b = i * 4 + (j + 1) % 4
            faces.append((a, b, b + 4, a + 4))
    faces.extend(((3, 2, 1, 0), tuple(range((len(path) - 1) * 4, len(path) * 4))))
    mesh = bpy.data.meshes.new(name)
    mesh.from_pydata(vertices, [], faces)
    mesh.update()
    obj = bpy.data.objects.new(name, mesh)
    bpy.context.scene.collection.objects.link(obj)
    return finish(obj, name, mat)


def pill(name, x, y, z, width, height, depth):
    radius = width / 2
    outline = []
    for top in (True, False):
        for i in range(25):
            angle = math.pi * i / 24 + (0 if top else math.pi)
            outline.append((radius * math.cos(angle),
                            (height / 2 - radius) * (1 if top else -1) + radius * math.sin(angle)))
    count = len(outline)
    vertices = [(px, side * depth / 2, pz) for side in (-1, 1) for px, pz in outline]
    faces = [tuple(range(count)), tuple(reversed(range(count, 2 * count)))]
    faces += [(i, count + i, count + (i + 1) % count, (i + 1) % count) for i in range(count)]
    mesh = bpy.data.meshes.new(name)
    mesh.from_pydata(vertices, [], faces)
    mesh.update()
    obj = bpy.data.objects.new(name, mesh)
    bpy.context.scene.collection.objects.link(obj)
    obj.location = (x, y, z)
    finish(obj, name, black)
    mesh.polygons[0].use_smooth = False
    mesh.polygons[1].use_smooth = False
    return obj


def cushion():
    vertices, faces = [], []
    for i in range(64):
        angle = math.tau * i / 64
        for j in range(10):
            tube = math.tau * j / 10
            vertices.append(((.081 + .008 * math.cos(tube)) * math.cos(angle),
                             .032 + .009 * math.sin(tube),
                             (.032 + .008 * math.cos(tube)) * math.sin(angle)))
    for i in range(64):
        for j in range(10):
            faces.append((i * 10 + j, ((i + 1) % 64) * 10 + j,
                          ((i + 1) % 64) * 10 + (j + 1) % 10, i * 10 + (j + 1) % 10))
    mesh = bpy.data.meshes.new('Cushion ring')
    mesh.from_pydata(vertices, [], faces)
    mesh.update()
    obj = bpy.data.objects.new('Facial cushion', mesh)
    bpy.context.scene.collection.objects.link(obj)
    finish(obj, 'Facial cushion', pad)


def rounded_shell():
    # A continuous superellipsoid rather than a bevelled box and flat faceplate.
    # Its broad oval front wraps into softly rounded sides, matching the Quest 3 profile.
    segments, rings = 80, 32
    a, b, c = .094, .036, .043
    exponent = .74
    def signed_power(value):
        return math.copysign(abs(value) ** exponent, value)
    vertices = [(0, 0, -c)]
    for row in range(1, rings):
        latitude = -math.pi / 2 + math.pi * row / rings
        cross = math.cos(latitude) ** exponent
        for i in range(segments):
            longitude = math.tau * i / segments
            vertices.append((a * cross * signed_power(math.cos(longitude)),
                             b * cross * signed_power(math.sin(longitude)),
                             c * signed_power(math.sin(latitude))))
    top = len(vertices)
    vertices.append((0, 0, c))
    faces = [(0, 1 + (i + 1) % segments, 1 + i) for i in range(segments)]
    for row in range(rings - 2):
        for i in range(segments):
            first = 1 + row * segments + i
            next_i = 1 + row * segments + (i + 1) % segments
            faces.append((first, next_i, next_i + segments, first + segments))
    faces += [(top, 1 + (rings - 2) * segments + i,
               1 + (rings - 2) * segments + (i + 1) % segments) for i in range(segments)]
    mesh = bpy.data.meshes.new('Rounded continuous shell')
    mesh.from_pydata(vertices, [], faces)
    mesh.update()
    obj = bpy.data.objects.new('Rounded Quest 3 housing', mesh)
    bpy.context.scene.collection.objects.link(obj)
    finish(obj, 'Rounded Quest 3 housing', face_white)
    return obj


def build(kind):
    scene = bpy.data.scenes.new(f'Linux Quest {kind}')
    bpy.context.window.scene = scene
    rounded_shell()
    cushion()
    for side in (-1, 1):
        box('Strap arm', (side * .091, .060, .004), (.014, .089, .021), white)
        box('Arm pivot', (side * .094, .019, .002), (.010, .026, .027), face_white)
    path = [(math.cos(math.pi * i / 48) * .098, .074 + math.sin(math.pi * i / 48) * .118, .004)
            for i in range(49)]
    ribbon('Rear fabric headband', path, .029, .0035, fabric, horizontal=True)
    path = [(0, -.003 + .195 * i / 40, .043 - .025 * i / 40 + .084 * math.sin(math.pi * i / 40)) for i in range(41)]
    ribbon('Top fabric strap', path, .026, .0035, fabric)
    for side in (-1, 1):
        ribbon('Top strap edge seam', [(side * .0115, p[1], p[2] + .0019) for p in path], .0007, .0005, seam)
    box('Strap adjuster', (0, .172, .043), (.034, .018, .009), face_white, .003)
    box('USB C port', (-.093, -.002, -.003), (.002, .014, .006), black, .002)
    box('Power button', (.093, -.001, -.003), (.002, .010, .004), white, .001)
    box('Volume rocker', (-.025, -.002, -.039), (.026, .007, .003), white, .001)
    if kind == 'quest-3':
        for index, x in enumerate((-.055, 0, .055)):
            before = set(scene.objects)
            pill(f'Sensor pill {index + 1}', x, -.0375, .001, .019, .048, .006)
            if index == 1:
                box('Depth projector window', (0, -.0410, .001), (.007, .0015, .021), glass, .003)
            else:
                for z in (-.0105, .0125):
                    lens('Optical retaining ring', x, -.0412, z, .0063, metal)
                    lens('Camera optical glass', x, -.0421, z, .0048, glass)
                    lens('Lens coating glint', x - .001, -.0430, z + .001, .0006, metal)
            theta = x * 2.4
            surface_y = -.036 * (1 - (abs(x) / .094) ** (2 / .74)) ** (.74 / 2)
            for obj in set(scene.objects) - before:
                local_x = obj.location.x - x
                local_y = obj.location.y + .0375
                obj.location.x = x + local_x * math.cos(theta) - local_y * math.sin(theta)
                obj.location.y = surface_y - .001 + local_x * math.sin(theta) + local_y * math.cos(theta)
                obj.rotation_euler.z += theta
        lens('Status LED', .030, -.035, .023, .0012, led)
    else:
        box('Neutral visor', (0, -.037, .001), (.155, .004, .049), black, .014)
    return scene


def export_runtime(scene, filename):
    bpy.context.view_layer.update()
    depsgraph = bpy.context.evaluated_depsgraph_get()
    depsgraph.update()
    records = []
    for obj in scene.objects:
        if obj.type != 'MESH':
            continue
        evaluated = obj.evaluated_get(depsgraph)
        mesh = evaluated.to_mesh()
        mesh.calc_loop_triangles()
        matrix = evaluated.matrix_world
        normals = matrix.to_3x3().inverted().transposed()
        for tri in mesh.loop_triangles:
            positions = []
            for index in tri.vertices:
                p = matrix @ mesh.vertices[index].co
                positions.extend((p.x, p.z, -p.y))
            # Export split normals per corner so the native renderer interpolates
            # the same smooth shell and fabric shading as Blender.
            corner_normals = []
            for index in tri.loops:
                n = (normals @ mesh.corner_normals[index].vector).normalized()
                corner_normals.extend((n.x, n.z, -n.y))
            color = obj.data.materials[tri.material_index].diffuse_color[:3]
            records.append(struct.pack('<21f', *positions, *corner_normals, *color))
        evaluated.to_mesh_clear()
    (assets / filename).write_bytes(b'LQM2' + struct.pack('<I', len(records)) + b''.join(records))
    print(f'{filename}: {len(records)} triangles')


quest = build('quest-3')
for obj in bpy.context.scene.objects:
    obj.select_set(obj.type == 'MESH')
bpy.ops.export_scene.gltf(filepath=str(assets / 'quest-3.glb'), export_format='GLB',
                          use_selection=True, export_apply=True, export_materials='EXPORT')
export_runtime(quest, 'quest-3.lqmesh')

# Product lighting and a transparent authoring preview.
world = bpy.data.worlds.new('Studio ambient')
world.use_nodes = True
world.node_tree.nodes['Background'].inputs[0].default_value = (.24, .28, .34, 1)
world.node_tree.nodes['Background'].inputs[1].default_value = .35
quest.world = world
for name, position, energy, size in (
    ('Key softbox', (.2, -.3, .4), 12, .32),
    ('Fill softbox', (-.3, -.1, .17), 7, .28),
    ('Rim softbox', (.1, .3, .3), 16, .22),
):
    data = bpy.data.lights.new(name, 'AREA')
    data.energy, data.shape, data.size = energy, 'DISK', size
    obj = bpy.data.objects.new(name, data)
    quest.collection.objects.link(obj)
    obj.location = position
    obj.rotation_euler = (Vector((0, .055, .025)) - obj.location).to_track_quat('-Z', 'Y').to_euler()
bpy.ops.object.camera_add(location=(.30, -.43, .24))
camera = bpy.context.object
camera.rotation_euler = (Vector((0, .06, .033)) - camera.location).to_track_quat('-Z', 'Y').to_euler()
camera.data.type = 'ORTHO'
camera.data.ortho_scale = .38
quest.camera = camera
quest.render.engine = 'CYCLES'
quest.cycles.samples = 40
quest.render.resolution_x, quest.render.resolution_y = 800, 640
quest.render.resolution_percentage = 100
quest.render.film_transparent = True
quest.render.image_settings.file_format = 'PNG'
quest.render.filepath = str(assets / 'quest-3-authoring-preview.png')
quest.view_settings.view_transform = 'AgX'
for screen in bpy.data.screens:
    for area in screen.areas:
        if area.type == 'VIEW_3D':
            area.spaces.active.region_3d.view_location = Vector((0, .06, .033))
            area.spaces.active.region_3d.view_distance = .42
            area.spaces.active.region_3d.view_rotation = camera.rotation_euler.to_quaternion()
bpy.ops.wm.save_as_mainfile(filepath=str(root / 'art/blender/quest-3.blend'))
bpy.ops.render.render(write_still=True)

generic = build('generic')
export_runtime(generic, 'generic.lqmesh')
