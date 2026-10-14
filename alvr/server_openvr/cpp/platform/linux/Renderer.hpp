#pragma once

#define VULKAN_HPP_NO_CONSTRUCTORS
#include <cstddef>
#include <vulkan/vulkan.h>
#include <vulkan/vulkan.hpp>
#include <vulkan/vulkan_core.h>
#include <vulkan/vulkan_enums.hpp>
#include <vulkan/vulkan_handles.hpp>
#include <vulkan/vulkan_structs.hpp>

#include "VkContext.hpp"
#include "utils.hpp"

#include "ffmpeg_helper.h"

namespace alvr::render {

constexpr u32 ImageCount = 6;
constexpr usize StagingImgCount = 2;

enum class HandleType {
    None,
    DmaBuf,
    OpaqueFd,
};

struct Image {
    vk::Image image = VK_NULL_HANDLE;
    vk::ImageLayout layout = vk::ImageLayout::eUndefined;
    vk::DeviceMemory memory = VK_NULL_HANDLE;
    vk::ImageView view = VK_NULL_HANDLE;

    void destroy(VkContext const& ctx) {
        ctx.dev.destroy(view);
        ctx.dev.destroy(image);
        ctx.dev.free(memory);
    }
};

struct Output {
    Image image;
    DrmImage drm;
    // VkSemaphore semaphore;
    VkImageCreateInfo imageCI;
    VkDeviceSize size;

    void destroy(VkContext const& ctx) {
        if (drm.fd >= 0) {
            close(drm.fd);
            drm.fd = -1;
        }
        image.destroy(ctx);
    }
};

struct PipelineCreateInfo {
    std::vector<u8> shaderData;
    std::vector<vk::SpecializationMapEntry> specs;
    std::vector<u8> specData;
};

struct RendererCreateInfo {
    vk::Format format;
    vk::Extent2D inputEyeExtent;
    vk::Extent2D outputExtent;
    std::array<int, ImageCount> inputImgFds;
};

// Push-constant payload for the rotational reprojection fold in the sampling
// shaders. Field offsets must match the GLSL block (0/64/80/96). enabled == 0
// reproduces the unwarped sampling exactly, so a zero-initialized struct is a
// correct passthrough.
struct WarpParams {
    float rotation[16] = {}; // column-major mat4, rotation in the upper-left 3x3
    float leftTans[4] = {}; // tanLeft, tanRight, tanUp, tanDown
    float rightTans[4] = {};
    u32 enabled = 0;
    u32 _padding[3] = {};
};
static_assert(sizeof(WarpParams) == 112);
static_assert(offsetof(WarpParams, rotation) == 0);
static_assert(offsetof(WarpParams, leftTans) == 64);
static_assert(offsetof(WarpParams, rightTans) == 80);
static_assert(offsetof(WarpParams, enabled) == 96);

// Region of an eye texture the app rendered into (vr::VRTextureBounds_t).
// Apps with adaptive resolution (SteamVR Home) render into a sub-rectangle and
// leave stale content in the rest; v may be flipped (vMin > vMax).
struct EyeBounds {
    float uMin = 0.f, vMin = 0.f, uMax = 1.f, vMax = 1.f;
    bool isFull() const { return uMin == 0.f && vMin == 0.f && uMax == 1.f && vMax == 1.f; }
};

// Per-eye foveation center shifts for ffr.comp, updated every frame.
struct FoveationCenterShifts {
    float left[2] = {};
    float right[2] = {};
};
static_assert(sizeof(FoveationCenterShifts) == 16);

// The push-constant range every pass shares. The warp sits at offset 0 so
// quad.comp can declare only that prefix; ffr.comp declares the whole block
// (centers at 112/120). Shaders that declare neither ignore it.
// Eye texture bounds (uMin, vMin, uMax, vMax) at 128/144 are read by quad.comp
// (160 bytes; within the 256-byte push constant limit of the supported GPUs).
struct PushConstants {
    WarpParams warp;
    FoveationCenterShifts centerShifts;
    float leftBounds[4] = { 0.f, 0.f, 1.f, 1.f };
    float rightBounds[4] = { 0.f, 0.f, 1.f, 1.f };
};
static_assert(sizeof(PushConstants) == 160);
static_assert(offsetof(PushConstants, centerShifts) == 112);
static_assert(offsetof(PushConstants, leftBounds) == 128);
static_assert(offsetof(PushConstants, rightBounds) == 144);

namespace detail {

    class RenderPipeline {
        vk::ShaderModule shader;
        vk::PipelineLayout pipeLayout;
        vk::Pipeline pipe;

    public:
        RenderPipeline(
            VkContext const& ctx, vk::DescriptorSetLayout& layout, PipelineCreateInfo& pipelineCI
        );

        void render(
            VkContext const& ctx,
            vk::CommandBuffer cmdBuf,
            vk::ImageView in,
            vk::ImageView out,
            vk::Extent2D outSize,
            PushConstants const& pushConstants
        );

        void destroy(VkContext const& ctx);
    };

}

class Renderer {
    vk::Extent2D eyeExtent;
    vk::Extent2D outExtent;

    Image inputImages[ImageCount];
    Image stagingImgs[StagingImgCount];
    Output output;

    vk::QueryPool timestampPool;
    vk::CommandPool cmdPool;
    vk::CommandBuffer cmdBuf;
    vk::Sampler sampler;
    vk::DescriptorSetLayout descLayout;
    vk::Fence fence;

    std::vector<detail::RenderPipeline> pipes;

    // vk::Semaphore renderFinishedSem;

public:
    Renderer(
        VkContext const& vkCtx,
        RendererCreateInfo& createInfo,
        std::vector<PipelineCreateInfo> pipeCIs
    );

    // TODO: Import output (somehow?)

    // NOTE: Use the output immediately afterwards, as this synchronizes to the end of gpu
    // operations
    // waitFds, when given, are sync_file fds imported as temporary semaphores
    // the submission waits on before the eye copies. Ownership transfers to
    // this call: a successful import hands the fd to Vulkan, a failed one is
    // closed here.
    void render(VkContext& vkCtx, u32 leftIdx, u32 rightIdx, int const waitFds[2] = nullptr);

    WarpParams warpParams {};

    EyeBounds eyeBounds[2] {};

    FoveationCenterShifts foveationCenterShifts {};

    Output getOutput() { return output; }

    void destroy(VkContext const& ctx);
};

}
