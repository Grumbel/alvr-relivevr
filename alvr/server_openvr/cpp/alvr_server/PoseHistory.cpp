#include "PoseHistory.h"
#include "Logger.h"
#include "Utils.h"
#include "include/openvr_math.h"
#include <algorithm>
#include <cmath>
#include <mutex>
#include <optional>

// A submitted frame was rendered from a recent pose, so matches older than this
// are stale no matter how well the rotation agrees.
static constexpr uint64_t MATCH_WINDOW_NS = 250000000;
// The head counts as still when all poses of this span agree within
// STILL_DISTANCE (squared matrix distance, about 0.2 degrees).
static constexpr uint64_t STILL_SPAN_NS = 100000000;
static constexpr float STILL_DISTANCE = 1e-5f;
static constexpr float CORRECTION_RATE = 0.2f;

static float RotationDistance(const vr::HmdMatrix34_t& a, const vr::HmdMatrix34_t& b) {
    float distance = 0;
    for (int i = 0; i < 3; i++) {
        for (int j = 0; j < 3; j++) {
            distance += pow(a.m[j][i] - b.m[j][i], 2);
        }
    }
    return distance;
}

// Gram-Schmidt on the rows, to keep a blended matrix a rotation.
static void Orthonormalize(float m[3][3]) {
    auto dot = [](const float* a, const float* b) { return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]; };
    auto normalize = [&](float* a) {
        float len = sqrtf(dot(a, a));
        for (int i = 0; i < 3; i++)
            a[i] /= len;
    };
    normalize(m[0]);
    float d = dot(m[1], m[0]);
    for (int i = 0; i < 3; i++)
        m[1][i] -= d * m[0][i];
    normalize(m[1]);
    m[2][0] = m[0][1] * m[1][2] - m[0][2] * m[1][1];
    m[2][1] = m[0][2] * m[1][0] - m[0][0] * m[1][2];
    m[2][2] = m[0][0] * m[1][1] - m[0][1] * m[1][0];
}

void PoseHistory::OnPoseUpdated(uint64_t targetTimestampNs, FfiDeviceMotion motion) {
    // Put pose history buffer
    TrackingHistoryFrame history;
    history.targetTimestampNs = targetTimestampNs;
    history.motion = motion;

    HmdMatrix_QuatToMat(
        motion.pose.orientation.w,
        motion.pose.orientation.x,
        motion.pose.orientation.y,
        motion.pose.orientation.z,
        &history.rotationMatrix
    );

    std::unique_lock<std::mutex> lock(m_mutex);
    if (!m_transformIdentity) {
        vr::HmdMatrix34_t rotation = vrmath::matMul33(m_transform, history.rotationMatrix);
        history.rotationMatrix = rotation;
    }

    if (m_poseBuffer.size() == 0) {
        m_poseBuffer.push_back(history);
    } else {
        if (m_poseBuffer.back().targetTimestampNs != targetTimestampNs) {
            // New track info
            m_poseBuffer.push_back(history);
        }
    }
    // The value should match with the client's MAXIMUM_TRACKING_FRAMES in ovr_context.cpp
    if (m_poseBuffer.size() > 120 * 3) {
        m_poseBuffer.pop_front();
    }
}

std::optional<PoseHistory::TrackingHistoryFrame>
PoseHistory::GetBestPoseMatch(const vr::HmdMatrix34_t& pose) const {
    std::unique_lock<std::mutex> lock(m_mutex);
    if (m_poseBuffer.empty()) {
        Debug("PoseHistory::GetBestPoseMatch: No pose matched.");
        return {};
    }

    // The buffer holds about 1.7 seconds of tracking, so sweeping the head back
    // over an orientation it recently held leaves several entries matching
    // equally well. Search newest first so a tie resolves to the most recent
    // entry, and stop at the window bound.
    const uint64_t newestTimestampNs = m_poseBuffer.back().targetTimestampNs;
    const auto& newest = m_poseBuffer.back().rotationMatrix;

    // Learn the correction while the head is still: then the app rendered with
    // (a prediction of) the newest pose, so pose = C * newest.
    bool still = true;
    for (auto it = m_poseBuffer.rbegin(); it != m_poseBuffer.rend(); ++it) {
        if (newestTimestampNs - it->targetTimestampNs > STILL_SPAN_NS)
            break;
        if (RotationDistance(it->rotationMatrix, newest) > STILL_DISTANCE) {
            still = false;
            break;
        }
    }
    if (still) {
        for (int i = 0; i < 3; i++) {
            for (int j = 0; j < 3; j++) {
                float measured = 0; // (pose * newest^T)[i][j]
                for (int k = 0; k < 3; k++)
                    measured += pose.m[i][k] * newest.m[j][k];
                m_correction[i][j] += CORRECTION_RATE * (measured - m_correction[i][j]);
            }
        }
        Orthonormalize(m_correction);
    }

    // Match C^T * pose against the history.
    vr::HmdMatrix34_t corrected = {};
    for (int i = 0; i < 3; i++) {
        for (int j = 0; j < 3; j++) {
            for (int k = 0; k < 3; k++)
                corrected.m[i][j] += m_correction[k][i] * pose.m[k][j];
        }
    }

    float minDiff = 100000;
    auto minIt = m_poseBuffer.rbegin();
    size_t index = 0, minIndex = 0;
    float secondDiff = 100000; // best distance among the other entries
    for (auto it = m_poseBuffer.rbegin(); it != m_poseBuffer.rend(); ++it, ++index) {
        if (newestTimestampNs - it->targetTimestampNs > MATCH_WINDOW_NS) {
            break;
        }
        // Rotation matrix composes a part of ViewMatrix of TrackingInfo.
        // The bottom and right side of the matrix are not compared, because
        // pPose does not contain that part of matrix.
        float distance = RotationDistance(it->rotationMatrix, corrected);
        if (minDiff > distance) {
            secondDiff = minDiff;
            minIt = it;
            minDiff = distance;
            minIndex = index;
        } else if (secondDiff > distance) {
            secondDiff = distance;
        }
    }
    static uint64_t calls = 0;
    if (++calls % 75 == 0) {
        Info(
            "PoseHistory: match %zu back, diff %.3g (next best %.3g), age %.1f ms, "
            "correction %.2f deg%s",
            minIndex,
            minDiff,
            secondDiff,
            (newestTimestampNs - minIt->targetTimestampNs) / 1e6,
            acosf(std::clamp(
                (m_correction[0][0] + m_correction[1][1] + m_correction[2][2] - 1) / 2, -1.0f, 1.0f
            )) * 180 / M_PI,
            still ? " (still)" : ""
        );
    }
    return *minIt;
}

std::optional<PoseHistory::TrackingHistoryFrame> PoseHistory::GetPoseAt(uint64_t timestampNs
) const {
    std::unique_lock<std::mutex> lock(m_mutex);
    for (auto it = m_poseBuffer.rbegin(), end = m_poseBuffer.rend(); it != end; ++it) {
        if (it->targetTimestampNs == timestampNs)
            return *it;
    }

    Debug("PoseHistory::GetPoseAt: No pose matched.");
    return {};
}

std::optional<PoseHistory::TrackingHistoryFrame> PoseHistory::GetLatestPose() const {
    std::unique_lock<std::mutex> lock(m_mutex);
    if (m_poseBuffer.empty()) {
        return {};
    }
    return m_poseBuffer.back();
}

void PoseHistory::SetTransform(const vr::HmdMatrix34_t& transform) {
    std::unique_lock<std::mutex> lock(m_mutex);
    m_transform = transform;

    for (int i = 0; i < 3; ++i) {
        for (int j = 0; j < 3; ++j) {
            if (transform.m[i][j] != (i == j ? 1 : 0)) {
                m_transformIdentity = false;
                return;
            }
        }
    }
    m_transformIdentity = true;
}
