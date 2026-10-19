#ifndef PARSER_CONFIG_H
#define PARSER_CONFIG_H

#include <string>

struct SmartnetParserConfig {
    int sys_num;
    std::string bandplan;
    int bandfreq;
    double bandplan_base;
    double bandplan_high;
    double bandplan_spacing;
    int bandplan_offset;
};

#endif
