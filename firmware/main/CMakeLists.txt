file(GLOB_RECURSE LIBS "lib/*/src/**.cpp" "lib/*/src/**.c")

file(GLOB_RECURSE SOURCES
        "${CMAKE_CURRENT_SOURCE_DIR}/src/**.cpp"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/**.hpp"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/**.c"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/**.h"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/*/**.cpp"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/*/**.hpp"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/*/**.c"
        "${CMAKE_CURRENT_SOURCE_DIR}/src/*/**.h"
        "${CMAKE_CURRENT_SOURCE_DIR}/main.cpp")

idf_component_register(SRCS ${SOURCES} ${LIBS} INCLUDE_DIRS "src")

spiffs_create_partition_image(storage ../spiffs FLASH_IN_PROJECT)

