#include "ogrsf_frmts.h"
#include <cstdint>
#include <exception>
#include <memory>
#include <vector>

namespace {
#if GDAL_VERSION_NUM < GDAL_COMPUTE_VERSION(3, 12, 0)
// Older OGR treats an empty definition as a request to copy every source field.
class GeometryOnlyDefn final : public OGRFeatureDefn {
  public:
    explicit GeometryOnlyDefn(const OGRFeatureDefn &output) : OGRFeatureDefn(output.GetName()) {
        SetGeomType(wkbNone);
        AddGeomFieldDefn(output.GetGeomFieldDefn(0));
        OGRFieldDefn unused("", OFTInteger);
        AddFieldDefn(&unused);
    }
    int GetFieldIndex(const char *) const override { return -1; }
};
#endif

// OGR layer algebra pushes features into a layer; the C API has no feature sink callback.
class OverlaySink final : public OGRLayer {
    OGRLayer &output_;
    const OGRwkbGeometryType multi_;
    const int dimension_;
    std::uint64_t &written_;

    struct Release {
        void operator()(OGRFeatureDefn *schema) const { schema->Release(); }
    };
    std::unique_ptr<OGRFeatureDefn, Release> native_schema_;
    std::vector<int> field_map_;

    OGRFeatureDefn *definition() const {
        return native_schema_ ? native_schema_.get() : output_.GetLayerDefn();
    }
    OGRErr write(OGRFeature *feature) {
        OGRErr status;
        if (native_schema_) {
            OGRFeature projected(output_.GetLayerDefn());
            status = projected.SetFrom(feature, field_map_.data(), FALSE);
            if (status == OGRERR_NONE)
                status = output_.CreateFeature(&projected);
        } else {
            status = output_.CreateFeature(feature);
        }
        if (status == OGRERR_NONE)
            ++written_;
        return status;
    }
    OGRErr ICreateFeature(OGRFeature *feature) override {
        const auto *g = feature->GetGeometryRef();
        if (!g) {
            CPLError(CE_Failure, CPLE_AppDefined, "NULL overlay result geometry");
            return OGRERR_FAILURE;
        }
        if (g->IsEmpty() || g->getDimension() < dimension_)
            return OGRERR_NONE;
        if (g->getDimension() > dimension_) {
            CPLError(CE_Failure, CPLE_AppDefined, "Unexpected overlay geometry dimension");
            return OGRERR_FAILURE;
        }
        if (g->getGeometryType() == multi_)
            return write(feature);

        // GEOS may return both a line and a point from one multiline intersection.
        auto *parts = g->getGeometryType() == wkbGeometryCollection
                          ? OGRGeometryFactory::removeLowerDimensionSubGeoms(g)
                          : g->clone();
        if (!parts)
            return OGRERR_NOT_ENOUGH_MEMORY;
        std::unique_ptr<OGRGeometry> normalized(
            OGRGeometry::FromHandle(OGR_G_ForceTo(OGRGeometry::ToHandle(parts), multi_, nullptr)));
        if (!normalized || normalized->getGeometryType() != multi_) {
            CPLError(CE_Failure, CPLE_AppDefined, "Unexpected overlay geometry family");
            return OGRERR_FAILURE;
        }
        const auto status = feature->SetGeometryDirectly(normalized.release());
        return status == OGRERR_NONE ? write(feature) : status;
    }

  public:
    OverlaySink(OGRLayer &output, OGRwkbGeometryType type, char **aliases, std::uint64_t &written)
        : output_(output), multi_(type), dimension_(type == wkbMultiPoint        ? 0
                                                    : type == wkbMultiLineString ? 1
                                                                                 : 2),
          written_(written) {
#if GDAL_VERSION_NUM < GDAL_COMPUTE_VERSION(3, 12, 0)
        if (output.GetLayerDefn()->GetFieldCount() == 0) {
            native_schema_.reset(new GeometryOnlyDefn(*output.GetLayerDefn()));
            native_schema_->Reference();
            field_map_.push_back(-1);
            return;
        }
#endif
        if (aliases && *aliases) {
            native_schema_.reset(output.GetLayerDefn()->Clone());
            if (!native_schema_)
                throw std::bad_alloc();
            native_schema_->Reference();
            for (int i = 0; i < native_schema_->GetFieldCount(); ++i) {
                native_schema_->GetFieldDefn(i)->SetName(aliases[i]);
                field_map_.push_back(i);
            }
        }
    }

    void ResetReading() override {}
    OGRFeature *GetNextFeature() override { return nullptr; }
#if GDAL_VERSION_NUM >= GDAL_COMPUTE_VERSION(3, 12, 0)
    const OGRFeatureDefn *GetLayerDefn() const override { return definition(); }
    int TestCapability(const char *) const override { return FALSE; }
#else
    OGRFeatureDefn *GetLayerDefn() override { return definition(); }
    int TestCapability(const char *) override { return FALSE; }
#endif
};
} // namespace

extern "C" unsigned spatial_core_overlay_header_version() noexcept { return GDAL_VERSION_NUM; }

extern "C" OGRErr spatial_core_overlay(decltype(&OGR_L_Intersection) operation, OGRLayerH input,
                                       OGRLayerH method, OGRLayerH output,
                                       OGRwkbGeometryType geometry_type, char **aliases,
                                       char **options, GDALProgressFunc progress, void *user_data,
                                       std::uint64_t *written) noexcept {
    try {
        OverlaySink sink(*OGRLayer::FromHandle(output), geometry_type, aliases, *written);
        return operation(input, method, OGRLayer::ToHandle(&sink), options, progress, user_data);
    } catch (const std::exception &error) {
        CPLError(CE_Failure, CPLE_AppDefined, "%s", error.what());
    } catch (...) {
        CPLError(CE_Failure, CPLE_AppDefined, "Unknown exception during OGR overlay");
    }
    return OGRERR_FAILURE;
}
