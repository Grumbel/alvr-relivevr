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
// A pose the app rendered with matches its history entry almost exactly
// (float noise only). Anything worse means the transform is off.
static constexpr float GOOD_MATCH_DISTANCE = 1e-6f;

static float RotationDistance(const vr::HmdMatrix34_t& a, const vr::HmdMatrix34_t& b) {
    float distance = 0;
    for (int i = 0; i < 3; i++) {
        for (int j = 0; j < 3; j++) {
            distance += pow(a.m[j][i] - b.m[j][i], 2);
        }
    }
    return distance;
}

// Rotation angle in degrees for a squared matrix distance (|A-B|^2 = 4(1-cos)).
static float DistanceToDegrees(float distance) {
    return acosf(std::clamp(1.0f - distance / 4.0f, -1.0f, 1.0f)) * 180 / M_PI;
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

    // Kept in raw tracking space: vrcompositor submits its layers with raw
    // poses, also after a SteamVR recenter changes the zero pose (applying
    // the zero pose here made every match after a recenter wrong).
    std::unique_lock<std::mutex> lock(m_mutex);

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
        float distance = RotationDistance(it->rotationMatrix, pose);
        if (minDiff > distance) {
            secondDiff = minDiff;
            minIt = it;
            minDiff = distance;
            minIndex = index;
        } else if (secondDiff > distance) {
            secondDiff = distance;
        }
    }
    static uint64_t calls = 0, bad = 0;
    ++calls;
    if (minDiff > GOOD_MATCH_DISTANCE) {
        // The frame gets the wrong pose: the warp starts from it and the
        // client reprojects from it, so the image jitters.
        if (bad++ % 75 == 0) {
            Warn(
                "PoseHistory: no exact pose match (%llu so far): best %.2f deg off, %.1f ms "
                "old",
                (unsigned long long)bad,
                DistanceToDegrees(minDiff),
                (newestTimestampNs - minIt->targetTimestampNs) / 1e6
            );
        }
    } else if (calls % 750 == 0) {
        Info(
            "PoseHistory: match %zu back, diff %.3g (next best %.3g), age %.1f ms, %llu inexact",
            minIndex,
            minDiff,
            secondDiff,
            (newestTimestampNs - minIt->targetTimestampNs) / 1e6,
            (unsigned long long)bad
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
