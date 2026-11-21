//! Just enough linear algebra to put a camera in front of a chunk.
//!
//! Column-major `[[f32; 4]; 4]` — index `m[column][row]` — because that is what
//! WGSL's `mat4x4<f32>` expects, so the uniform upload is a straight copy.
//! Column-major also means the matrix rows are `[m[0][r], m[1][r], m[2][r],
//! m[3][r]]`, which is the form every function below is written in.

pub type Mat4 = [[f32; 4]; 4];

pub fn identity() -> Mat4 {
    let mut m = [[0.0f32; 4]; 4];
    m[0][0] = 1.0;
    m[1][1] = 1.0;
    m[2][2] = 1.0;
    m[3][3] = 1.0;
    m
}

/// `a * b`: applies `b` first, then `a`.
pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k][row] * b[col][k];
            }
            out[col][row] = sum;
        }
    }
    out
}

/// Right-handed perspective with a **0..1** depth range, which is what wgpu
/// wants; OpenGL's -1..1 version would put half the world behind the near plane.
pub fn perspective(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y_radians / 2.0).tan();
    let mut m = [[0.0f32; 4]; 4];
    m[0][0] = f / aspect.max(1e-6);
    m[1][1] = f;
    m[2][2] = far / (near - far);
    m[2][3] = -1.0;
    m[3][2] = (far * near) / (near - far);
    m
}

pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len < 1e-6 {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

pub fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> Mat4 {
    let f = normalize(sub(target, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    let mut m = identity();
    m[0][0] = s[0];
    m[1][0] = s[1];
    m[2][0] = s[2];
    m[3][0] = -dot(s, eye);
    m[0][1] = u[0];
    m[1][1] = u[1];
    m[2][1] = u[2];
    m[3][1] = -dot(u, eye);
    m[0][2] = -f[0];
    m[1][2] = -f[1];
    m[2][2] = -f[2];
    m[3][2] = dot(f, eye);
    m
}

/// Camera described the way a person thinks about it: where it looks from, and
/// what it looks at.
///
/// Stage 2 replaced this with [`FlyCamera`]. It is kept, and compiled into test
/// builds only, because `a_fly_camera_reproduces_the_orbit_view` uses it as the
/// independent reference: the fly camera has to reproduce the stage 1 view, and
/// that test is what proves it instead of a screenshot that looks similar.
#[cfg_attr(not(test), allow(dead_code))]
pub struct Camera {
    pub eye: [f32; 3],
    pub target: [f32; 3],
    pub fov_y_degrees: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Camera {
    /// Place the camera on a sphere around `target`: `yaw` in degrees around Y,
    /// `pitch` above the horizon, `distance` away.
    pub fn orbit(target: [f32; 3], yaw_degrees: f32, pitch_degrees: f32, distance: f32) -> Self {
        let yaw = yaw_degrees.to_radians();
        let pitch = pitch_degrees.to_radians();
        let eye = [
            target[0] + distance * yaw.cos() * pitch.cos(),
            target[1] + distance * pitch.sin(),
            target[2] + distance * yaw.sin() * pitch.cos(),
        ];
        Camera {
            eye,
            target,
            fov_y_degrees: 60.0,
        }
    }

    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        let view = look_at(self.eye, self.target, [0.0, 1.0, 0.0]);
        // A far plane big enough for a 32×32 chunk region; the depth buffer is
        // 32-bit float, so the precision loss at the far end does not matter at
        // this scale.
        let proj = perspective(self.fov_y_degrees.to_radians(), aspect, 0.1, 2_000.0);
        mul(&proj, &view)
    }
}

/// A free-fly camera: a position and two angles, no orbit target.
///
/// `yaw` and `pitch` use the same convention as [`Camera::orbit`], and
/// [`FlyCamera::from_orbit`] lands on exactly the same view — which is how the
/// first frame of stage 2 was checked against the stage 1 screenshot instead of
/// being eyeballed.
pub struct FlyCamera {
    pub eye: [f32; 3],
    /// Degrees. Increasing yaw turns **right**, matching a mouse moved right.
    pub yaw_degrees: f32,
    /// Degrees. Positive looks **down**, so a camera above its target looking at
    /// it has a positive pitch. Clamped to ±89 so the forward vector never
    /// degenerates and the view never rolls.
    pub pitch_degrees: f32,
    pub fov_y_degrees: f32,
}

impl FlyCamera {
    pub fn new(eye: [f32; 3], yaw_degrees: f32, pitch_degrees: f32) -> Self {
        FlyCamera {
            eye,
            yaw_degrees,
            pitch_degrees,
            fov_y_degrees: 60.0,
        }
    }

    /// The view an orbit camera at these parameters has, expressed as a fly
    /// camera standing at the orbit's eye.
    pub fn from_orbit(target: [f32; 3], yaw: f32, pitch: f32, distance: f32) -> Self {
        let orbit = Camera::orbit(target, yaw, pitch, distance);
        let mut cam = FlyCamera::new(orbit.eye, yaw, pitch);
        cam.fov_y_degrees = orbit.fov_y_degrees;
        cam
    }

    pub fn forward(&self) -> [f32; 3] {
        let yaw = self.yaw_degrees.to_radians();
        let pitch = self.pitch_degrees.to_radians();
        [
            -yaw.cos() * pitch.cos(),
            -pitch.sin(),
            -yaw.sin() * pitch.cos(),
        ]
    }

    /// Unit vector along +X of the screen: `forward × up`, which is what makes
    /// "right" mean right rather than left.
    pub fn right(&self) -> [f32; 3] {
        normalize(cross(self.forward(), [0.0, 1.0, 0.0]))
    }

    /// Apply a mouse movement in pixels. `dy` is winit's sign, where moving the
    /// mouse **up** is negative.
    pub fn turn(&mut self, dx: f32, dy: f32, degrees_per_pixel: f32) {
        self.yaw_degrees = (self.yaw_degrees + dx * degrees_per_pixel).rem_euclid(360.0);
        self.pitch_degrees =
            (self.pitch_degrees + dy * degrees_per_pixel).clamp(-89.0, 89.0);
    }

    /// Move in the camera's own frame: `forward` blocks along the view, `strafe`
    /// to the right, `rise` straight up in world space.
    pub fn fly(&mut self, forward: f32, strafe: f32, rise: f32) {
        if forward == 0.0 && strafe == 0.0 && rise == 0.0 {
            return;
        }
        let f = self.forward();
        let r = self.right();
        for i in 0..3 {
            self.eye[i] += f[i] * forward + r[i] * strafe;
        }
        self.eye[1] += rise;
    }

    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        let f = self.forward();
        let target = [self.eye[0] + f[0], self.eye[1] + f[1], self.eye[2] + f[2]];
        let view = look_at(self.eye, target, [0.0, 1.0, 0.0]);
        let proj = perspective(self.fov_y_degrees.to_radians(), aspect, 0.1, 2_000.0);
        mul(&proj, &view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &Mat4, v: [f32; 4]) -> [f32; 4] {
        let mut out = [0.0f32; 4];
        for row in 0..4 {
            out[row] = (0..4).map(|k| m[k][row] * v[k]).sum();
        }
        out
    }

    #[test]
    fn identity_leaves_a_point_alone() {
        let p = apply(&identity(), [1.0, 2.0, 3.0, 1.0]);
        assert_eq!(p, [1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn look_at_puts_the_target_in_front_of_the_camera() {
        // Camera at +Z looking at the origin: the target must land on -Z in view
        // space, i.e. in front of a right-handed camera.
        let view = look_at([0.0, 0.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let p = apply(&view, [0.0, 0.0, 0.0, 1.0]);
        assert!((p[2] + 10.0).abs() < 1e-4, "view-space z was {}", p[2]);
        assert!(p[0].abs() < 1e-4 && p[1].abs() < 1e-4);
    }

    #[test]
    fn perspective_maps_the_near_plane_to_zero_and_far_to_one() {
        let proj = perspective(60f32.to_radians(), 1.0, 0.1, 100.0);
        // A point on the near plane, in front of the camera along -Z.
        let near = apply(&proj, [0.0, 0.0, -0.1, 1.0]);
        let far = apply(&proj, [0.0, 0.0, -100.0, 1.0]);
        let near_ndc = near[2] / near[3];
        let far_ndc = far[2] / far[3];
        assert!(near_ndc.abs() < 1e-4, "near depth {near_ndc}");
        assert!((far_ndc - 1.0).abs() < 1e-4, "far depth {far_ndc}");
    }

    #[test]
    fn a_point_to_the_right_is_on_the_right_of_the_screen() {
        let camera = Camera::orbit([0.0, 0.0, 0.0], 0.0, 0.0, 10.0);
        let vp = camera.view_projection(1.0);
        // Camera sits at +X looking towards -X, so +Z is to its right.
        let right = apply(&vp, [0.0, 0.0, -5.0, 1.0]);
        let ndc_x = right[0] / right[3];
        assert!(ndc_x > 0.0, "expected right of centre, got {ndc_x}");
    }

    /// The stage 1 screenshot came from an orbit camera. A fly camera built from
    /// the same numbers must produce the same image, or stage 2 changed the view
    /// while claiming not to.
    #[test]
    fn a_fly_camera_reproduces_the_orbit_view() {
        let target = [104.0, 68.0, -184.0];
        let orbit = Camera::orbit(target, 40.0, 30.0, 60.0);
        let fly = FlyCamera::from_orbit(target, 40.0, 30.0, 60.0);
        let a = orbit.view_projection(1600.0 / 900.0);
        let b = fly.view_projection(1600.0 / 900.0);
        for col in 0..4 {
            for row in 0..4 {
                // Relative, because the translation column holds values around
                // 135 and f32 has ~7 digits: the two paths normalise the view
                // direction differently and land 1e-4 apart in absolute terms.
                let scale = a[col][row].abs().max(1.0);
                let diff = (a[col][row] - b[col][row]).abs();
                assert!(
                    diff < 1e-4 * scale,
                    "matrix differs at [{col}][{row}]: {} vs {} (diff {diff})",
                    a[col][row],
                    b[col][row]
                );
            }
        }
    }

    #[test]
    fn flying_forward_follows_the_view_direction() {
        // yaw 0, pitch 0 looks along -X.
        let mut cam = FlyCamera::new([0.0, 0.0, 0.0], 0.0, 0.0);
        cam.fly(5.0, 0.0, 0.0);
        assert!((cam.eye[0] + 5.0).abs() < 1e-4, "{:?}", cam.eye);
        assert!(cam.eye[1].abs() < 1e-4 && cam.eye[2].abs() < 1e-4, "{:?}", cam.eye);
    }

    #[test]
    fn strafing_right_moves_along_negative_z_when_facing_negative_x() {
        let mut cam = FlyCamera::new([0.0, 0.0, 0.0], 0.0, 0.0);
        cam.fly(0.0, 2.0, 0.0);
        // Facing -X with +Y up, the right hand points to -Z.
        assert!((cam.eye[2] + 2.0).abs() < 1e-4, "{:?}", cam.eye);
    }

    #[test]
    fn pitch_is_clamped_and_yaw_wraps() {
        let mut cam = FlyCamera::new([0.0, 0.0, 0.0], 350.0, 0.0);
        cam.turn(0.0, 1000.0, 0.1);
        assert_eq!(cam.pitch_degrees, 89.0);
        cam.turn(0.0, -20000.0, 0.1);
        assert_eq!(cam.pitch_degrees, -89.0);
        cam.turn(100.0, 0.0, 1.0);
        assert!((0.0..360.0).contains(&cam.yaw_degrees), "{}", cam.yaw_degrees);
    }

    #[test]
    fn a_mouse_moved_right_turns_right() {
        // Facing -X (yaw 0) with +Z to the right, turning right must head for -Z.
        let mut cam = FlyCamera::new([0.0, 0.0, 0.0], 0.0, 0.0);
        cam.turn(10.0, 0.0, 1.0);
        assert_eq!(cam.yaw_degrees, 10.0);
        let f = cam.forward();
        assert!(f[2] < -0.1, "forward should bend towards -Z, got {f:?}");
    }
}
