//! Pure-Rust 3D vector, quaternion, and 6DoF spatial pose primitives.

use serde::{Deserialize, Serialize};

/// 3D Vector in meters representing a spatial coordinate or translation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    pub const UNIT_X: Self = Self {
        x: 1.0,
        y: 0.0,
        z: 0.0,
    };
    pub const UNIT_Y: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };
    pub const UNIT_Z: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 1.0,
    };

    /// Creates a new 3D vector.
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// Length (Euclidean norm) of the vector.
    pub fn length(&self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Squared length of the vector.
    pub fn length_squared(&self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// Returns normalized unit vector, or `ZERO` if length is negligible.
    pub fn normalize(&self) -> Self {
        let len = self.length();
        if len > 1e-6 {
            Self {
                x: self.x / len,
                y: self.y / len,
                z: self.z / len,
            }
        } else {
            Self::ZERO
        }
    }

    /// Dot product of two vectors.
    pub fn dot(&self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Cross product of two vectors.
    pub fn cross(&self, other: Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }
}

impl Default for Vec3 {
    fn default() -> Self {
        Self::ZERO
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
            z: self.z + rhs.z,
        }
    }
}

impl std::ops::AddAssign for Vec3 {
    fn add_assign(&mut self, rhs: Self) {
        self.x += rhs.x;
        self.y += rhs.y;
        self.z += rhs.z;
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
            z: self.z - rhs.z,
        }
    }
}

impl std::ops::SubAssign for Vec3 {
    fn sub_assign(&mut self, rhs: Self) {
        self.x -= rhs.x;
        self.y -= rhs.y;
        self.z -= rhs.z;
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self {
            x: self.x * rhs,
            y: self.y * rhs,
            z: self.z * rhs,
        }
    }
}

impl std::ops::Div<f32> for Vec3 {
    type Output = Self;
    fn div(self, rhs: f32) -> Self::Output {
        Self {
            x: self.x / rhs,
            y: self.y / rhs,
            z: self.z / rhs,
        }
    }
}

impl std::ops::Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self::Output {
        Self {
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }
}

impl From<[f32; 3]> for Vec3 {
    fn from(arr: [f32; 3]) -> Self {
        Self {
            x: arr[0],
            y: arr[1],
            z: arr[2],
        }
    }
}

impl From<Vec3> for [f32; 3] {
    fn from(v: Vec3) -> Self {
        [v.x, v.y, v.z]
    }
}

impl From<openxr::Vector3f> for Vec3 {
    fn from(v: openxr::Vector3f) -> Self {
        Self {
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }
}

impl From<Vec3> for openxr::Vector3f {
    fn from(v: Vec3) -> Self {
        openxr::Vector3f {
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }
}

/// Unit quaternion representing a 3D spatial orientation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quat {
    pub const IDENTITY: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };

    /// Creates a quaternion from raw components.
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// Constructs a rotation quaternion from Euler angles in radians: pitch (X), yaw (Y), roll (Z).
    ///
    /// Applies yaw (global Y) then pitch (local X) then roll (local Z).
    pub fn from_euler(pitch_rad: f32, yaw_rad: f32, roll_rad: f32) -> Self {
        let cp = (pitch_rad * 0.5).cos();
        let sp = (pitch_rad * 0.5).sin();
        let cy = (yaw_rad * 0.5).cos();
        let sy = (yaw_rad * 0.5).sin();
        let cr = (roll_rad * 0.5).cos();
        let sr = (roll_rad * 0.5).sin();

        let q_yaw = Self::new(0.0, sy, 0.0, cy);
        let q_pitch = Self::new(sp, 0.0, 0.0, cp);
        let q_roll = Self::new(0.0, 0.0, sr, cr);

        q_yaw * q_pitch * q_roll
    }

    /// Extracts Euler angles (pitch_rad, yaw_rad, roll_rad) in radians from the quaternion.
    pub fn to_euler(&self) -> (f32, f32, f32) {
        let q = self.normalize();

        // Pitch (rotation around X)
        let sin_p = 2.0 * (q.w * q.x - q.y * q.z);
        let pitch = if sin_p.abs() >= 1.0 {
            std::f32::consts::FRAC_PI_2.copysign(sin_p)
        } else {
            sin_p.asin()
        };

        // Yaw (rotation around Y)
        let sin_y = 2.0 * (q.w * q.y + q.x * q.z);
        let cos_y = 1.0 - 2.0 * (q.x * q.x + q.y * q.y);
        let yaw = sin_y.atan2(cos_y);

        // Roll (rotation around Z)
        let sin_r = 2.0 * (q.w * q.z + q.x * q.y);
        let cos_r = 1.0 - 2.0 * (q.x * q.x + q.z * q.z);
        let roll = sin_r.atan2(cos_r);

        (pitch, yaw, roll)
    }

    /// Constructs a rotation quaternion from an axis and angle in radians.
    pub fn from_axis_angle(axis: Vec3, angle_rad: f32) -> Self {
        let norm_axis = axis.normalize();
        let half_angle = angle_rad * 0.5;
        let s = half_angle.sin();
        Self {
            x: norm_axis.x * s,
            y: norm_axis.y * s,
            z: norm_axis.z * s,
            w: half_angle.cos(),
        }
    }

    /// Squared norm of the quaternion.
    pub fn norm_squared(&self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w
    }

    /// Normalizes the quaternion to unit length.
    pub fn normalize(&self) -> Self {
        let norm = self.norm_squared().sqrt();
        if norm > 1e-6 {
            Self {
                x: self.x / norm,
                y: self.y / norm,
                z: self.z / norm,
                w: self.w / norm,
            }
        } else {
            Self::IDENTITY
        }
    }

    /// Returns the quaternion conjugate (inverse for unit quaternions).
    pub fn conjugate(&self) -> Self {
        Self {
            x: -self.x,
            y: -self.y,
            z: -self.z,
            w: self.w,
        }
    }

    /// Rotates a 3D vector by this quaternion: `q * v * q^-1`.
    pub fn rotate_vec(&self, v: Vec3) -> Vec3 {
        let q_vec = Vec3::new(self.x, self.y, self.z);
        let uv = q_vec.cross(v);
        let uuv = q_vec.cross(uv);
        v + (uv * self.w + uuv) * 2.0
    }
}

impl Default for Quat {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl std::ops::Mul for Quat {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self::Output {
        Self {
            x: self.w * rhs.x + self.x * rhs.w + self.y * rhs.z - self.z * rhs.y,
            y: self.w * rhs.y - self.x * rhs.z + self.y * rhs.w + self.z * rhs.x,
            z: self.w * rhs.z + self.x * rhs.y - self.y * rhs.x + self.z * rhs.w,
            w: self.w * rhs.w - self.x * rhs.x - self.y * rhs.y - self.z * rhs.z,
        }
    }
}

impl From<[f32; 4]> for Quat {
    fn from(arr: [f32; 4]) -> Self {
        Self {
            x: arr[0],
            y: arr[1],
            z: arr[2],
            w: arr[3],
        }
    }
}

impl From<Quat> for [f32; 4] {
    fn from(q: Quat) -> Self {
        [q.x, q.y, q.z, q.w]
    }
}

impl From<openxr::Quaternionf> for Quat {
    fn from(q: openxr::Quaternionf) -> Self {
        Self {
            x: q.x,
            y: q.y,
            z: q.z,
            w: q.w,
        }
    }
}

impl From<Quat> for openxr::Quaternionf {
    fn from(q: Quat) -> Self {
        openxr::Quaternionf {
            x: q.x,
            y: q.y,
            z: q.z,
            w: q.w,
        }
    }
}

/// 6DoF spatial pose combining a 3D position and orientation quaternion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ScreenPose {
    /// 3D position in meters.
    pub position: Vec3,
    /// 3D orientation quaternion.
    pub orientation: Quat,
}

impl ScreenPose {
    pub const IDENTITY: Self = Self {
        position: Vec3::ZERO,
        orientation: Quat::IDENTITY,
    };

    pub fn new(position: Vec3, orientation: Quat) -> Self {
        Self {
            position,
            orientation,
        }
    }
}

impl From<openxr::Posef> for ScreenPose {
    fn from(p: openxr::Posef) -> Self {
        Self {
            position: p.position.into(),
            orientation: p.orientation.into(),
        }
    }
}

impl From<ScreenPose> for openxr::Posef {
    fn from(p: ScreenPose) -> Self {
        openxr::Posef {
            orientation: p.orientation.into(),
            position: p.position.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vec3_operations() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);

        assert_eq!(a + b, Vec3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vec3::new(3.0, 3.0, 3.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(b / 2.0, Vec3::new(2.0, 2.5, 3.0));
        assert_eq!(-a, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(a.dot(b), 1.0 * 4.0 + 2.0 * 5.0 + 3.0 * 6.0);

        let cross = Vec3::UNIT_X.cross(Vec3::UNIT_Y);
        assert!((cross - Vec3::UNIT_Z).length() < 1e-6);

        let norm = Vec3::new(0.0, 3.0, 4.0).normalize();
        assert!((norm.length() - 1.0).abs() < 1e-6);
        assert_eq!(norm.y, 0.6);
        assert_eq!(norm.z, 0.8);
    }

    #[test]
    fn test_quat_euler_roundtrip() {
        let pitch = 0.25; // ~14.3 degrees
        let yaw = -0.50; // ~ -28.6 degrees
        let roll = 0.0;

        let q = Quat::from_euler(pitch, yaw, roll);
        let (p_out, y_out, r_out) = q.to_euler();

        assert!((p_out - pitch).abs() < 1e-4);
        assert!((y_out - yaw).abs() < 1e-4);
        assert!(r_out.abs() < 1e-4);
    }

    #[test]
    fn test_quat_vector_rotation() {
        // 90 degrees around Y axis rotates UNIT_Z into UNIT_X
        let q = Quat::from_euler(0.0, std::f32::consts::FRAC_PI_2, 0.0);
        let rotated = q.rotate_vec(Vec3::UNIT_Z);
        assert!((rotated.x - 1.0).abs() < 1e-5);
        assert!(rotated.y.abs() < 1e-5);
        assert!(rotated.z.abs() < 1e-5);
    }

    #[test]
    fn test_openxr_conversions() {
        let v = Vec3::new(1.0, 2.0, -3.0);
        let xr_v: openxr::Vector3f = v.into();
        assert_eq!(xr_v.x, 1.0);
        assert_eq!(xr_v.y, 2.0);
        assert_eq!(xr_v.z, -3.0);
        let back_v: Vec3 = xr_v.into();
        assert_eq!(back_v, v);

        let q = Quat::new(0.0, 0.707, 0.0, 0.707);
        let xr_q: openxr::Quaternionf = q.into();
        assert_eq!(xr_q.y, 0.707);
        let back_q: Quat = xr_q.into();
        assert_eq!(back_q, q);

        let pose = ScreenPose::new(v, q);
        let xr_pose: openxr::Posef = pose.into();
        assert_eq!(xr_pose.position.z, -3.0);
        let back_pose: ScreenPose = xr_pose.into();
        assert_eq!(back_pose, pose);
    }
}
