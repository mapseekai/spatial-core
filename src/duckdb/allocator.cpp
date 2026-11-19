#include "duckdb/common/allocator.hpp"

#ifdef DUCKDB_DEBUG_ALLOCATION
#error "Spatial supports the default DuckDB Debug allocator, not DUCKDB_DEBUG_ALLOCATION"
#endif

extern "C" bool spatial_core_duckdb_debug_allocator() noexcept {
    const auto data = duckdb::Allocator::DefaultAllocator().GetPrivateData();
    return data && data->debug_info;
}
