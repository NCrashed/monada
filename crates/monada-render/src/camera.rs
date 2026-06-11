//! A simple orbit camera for the M1 top-down view.
//!
//! roxlap's [`Camera`] is a position plus an orthonormal `right/down/
//! forward` basis in the voxlap z-down world. We expose an orbit around
//! a fixed look-at point (the circle centre) parameterised by yaw,
//! pitch, and distance, and convert to that basis with the exact
//! yaw/pitch formula the roxlap host uses — so the basis is guaranteed
//! consistent with the projection the renderer applies.

use glam::DVec3;
use roxlap_core::Camera;

/// Orbit camera: looks at `center` from `dist` away, at `yaw`/`pitch`.
#[derive(Clone, Copy, Debug)]
pub struct OrbitCamera {
    pub center: DVec3,
    /// Rotation about the world z axis (radians).
    pub yaw: f64,
    /// Tilt below the horizon (radians); `pi/2` looks straight down.
    pub pitch: f64,
    /// Eye distance from `center`, in world voxels.
    pub dist: f64,
}

impl OrbitCamera {
    const PITCH_MIN: f64 = 0.25;
    const PITCH_MAX: f64 = 1.45;
    const DIST_MIN: f64 = 60.0;
    const DIST_MAX: f64 = 2000.0;

    /// A high-angle view that frames the circle: looking roughly
    /// "north-and-down" from far enough out that the ~96-voxel cloud
    /// sits well inside the 90° horizontal FOV.
    #[must_use]
    pub fn framing(center: DVec3) -> OrbitCamera {
        OrbitCamera {
            center,
            yaw: 0.0,
            pitch: 1.1,
            dist: 300.0,
        }
    }

    /// Nudge the orbit; pitch and distance are clamped to sane ranges.
    pub fn orbit(&mut self, dyaw: f64, dpitch: f64, ddist: f64) {
        self.yaw += dyaw;
        self.pitch = (self.pitch + dpitch).clamp(Self::PITCH_MIN, Self::PITCH_MAX);
        self.dist = (self.dist + ddist).clamp(Self::DIST_MIN, Self::DIST_MAX);
    }

    /// Convert to roxlap's `pos` + `right/down/forward` basis.
    ///
    /// `forward` is the view direction; the eye sits `dist` *behind* the
    /// look-at along it. The basis is **right-handed** (`right × down =
    /// forward`), matching the voxlap `setcamera` convention used by the
    /// sprite oracle. This matters: the sprite frustum cull derives its
    /// inward edge normals from the corner winding, so a left-handed
    /// basis (which the grid opticast tolerates) makes the cull reject
    /// every sprite. At yaw = pitch = 0 this yields `forward = +x`,
    /// `right = +y`, `down = +z` — exactly the oracle pose.
    #[must_use]
    pub fn to_roxlap(&self) -> Camera {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();

        let forward = [cy * cp, sy * cp, sp];
        let right = [-sy, cy, 0.0];
        let down = [-sp * cy, -sp * sy, cp];

        let fwd = DVec3::from_array(forward);
        let eye = self.center - fwd * self.dist;

        Camera {
            pos: eye.to_array(),
            right,
            down,
            forward,
        }
    }
}
