
#include "p25_recorder_decode.h"
#include "../systems/system.h"
#include "../event_sink.h"
#include "../gr_blocks/plugin_wrapper_impl.h"
#include "../formatter.h"
#include "../unit_tags_ota.h"
#include <algorithm>
#include <chrono>

p25_recorder_decode_sptr make_p25_recorder_decode(const std::shared_ptr<Recorder> &recorder, const Config &config) {
  auto decoder = new p25_recorder_decode(recorder, config);
  decoder->initialize();
  return gnuradio::get_initial_sptr(decoder);
}

p25_recorder_decode::p25_recorder_decode(const std::shared_ptr<Recorder> &recorder, const Config &config)
    : gr::hier_block2("p25_recorder_decode",
                      gr::io_signature::make(1, 1, sizeof(int16_t)),
                      gr::io_signature::make(0, 0, sizeof(float))),
      d_config(config),
      d_source_id(-1) {
  d_recorder = recorder;
}

p25_recorder_decode::~p25_recorder_decode() {
}

void p25_recorder_decode::stop() {
  wav_sink->stop_recording();
  std::lock_guard<std::mutex> lock(d_state_mutex);
  d_system = nullptr;
  d_source_id = -1;
}

std::vector<long> p25_recorder_decode::call_sources() const {
  std::lock_guard<std::mutex> lock(d_state_mutex);
  return d_call_sources;
}

void p25_recorder_decode::flush_plugin() {
  plugin_sink->flush();
}

void p25_recorder_decode::set_system(const std::shared_ptr<System> &sys) {
  std::lock_guard<std::mutex> lock(d_state_mutex);
  d_system = sys;
}

void p25_recorder_decode::start(const RecorderConfig &config) {
  levels->set_k(config.digital_levels);
  {
    std::lock_guard<std::mutex> lock(d_state_mutex);
    d_source_id = -1;
    d_call_sources.clear();
  }
  if (config.phase2_tdma) {
    wav_sink->start_recording(config, config.tdma_slot);
  } else {
    wav_sink->start_recording(config);
  }
}

void p25_recorder_decode::set_source(long src) {
  {
    std::lock_guard<std::mutex> lock(d_state_mutex);
    d_source_id = src;
  }
  wav_sink->set_source(src);
}

std::vector<Transmission> p25_recorder_decode::get_transmission_list() {
  return wav_sink->get_transmission_list();
}

RecorderState p25_recorder_decode::get_state() {
  return wav_sink->get_state();
}

std::chrono::duration<double> p25_recorder_decode::since_last_write() {
  return std::chrono::steady_clock::now() - wav_sink->get_last_write_time();
}

std::chrono::duration<double> p25_recorder_decode::since_last_voice() {
  return std::chrono::steady_clock::now() - wav_sink->get_last_voice_time();
}

void p25_recorder_decode::initialize() {
  wav_sink = gr::blocks::headless_sink::make(1, 8000, 16);
  levels = gr::blocks::multiply_const_ss::make(1);
  plugin_sink = gr::blocks::plugin_wrapper_impl::make(std::bind(&p25_recorder_decode::plugin_callback_handler, this, std::placeholders::_1, std::placeholders::_2));
  plugin_sink->set_source_id_callback(std::bind(&p25_recorder_decode::source_id_handler, this, std::placeholders::_1));

  connect(self(), 0, levels, 0);
  connect(levels, 0, plugin_sink, 0);
  connect(levels, 0, wav_sink, 0);
}

void p25_recorder_decode::handle_json(const std::string &s) {
  try {
    auto j = nlohmann::json::parse(s);
    if (!j.contains("type")) return;
    std::string t = j["type"];
    if (t == "motorola_alias_p1" || t == "motorola_alias_p2" ||
        t == "harris_alias_p1" || t == "harris_alias_p2") {
      handle_alias_message(j);
    }
  } catch (const std::exception &e) {
    BOOST_LOG_TRIVIAL(debug) << "Malformed P25 JSON message: " << e.what();
  }
}

void p25_recorder_decode::plugin_callback_handler(int16_t *samples, int sampleCount) {
  d_config.event_sink->audio_callback(d_recorder, samples, sampleCount);
}

void p25_recorder_decode::source_id_handler(long src_id) {
  // Report in-band source (unit) changes up to the Rust state machine so it can
  // cut a per-speaker transmission boundary. The tag re-fires every superframe
  // with the same src, so only forward an actual change.
  if (src_id > 0) {
    std::lock_guard<std::mutex> lock(d_state_mutex);
    if (std::find(d_call_sources.begin(), d_call_sources.end(), src_id) == d_call_sources.end()) d_call_sources.push_back(src_id);
  }
  if (src_id > 0 && src_id != d_last_notified_src) {
    d_last_notified_src = src_id;
    if (d_recorder) {
      d_recorder->notify_source_change(src_id);
    }
  }
}

double p25_recorder_decode::get_output_sample_rate() {
  return 8000;
}

void p25_recorder_decode::handle_alias_message(const nlohmann::json& j) {
  std::lock_guard<std::mutex> lock(d_state_mutex);
  int messages = j.contains("messages") ? j["messages"].get<int>() : 0;
  std::array<std::vector<uint8_t>, 10> alias_buffer;
  
  // Decode hex strings back to binary (all formats use blocks)
  if (j.contains("blocks")) {
    for (auto& [key, value] : j["blocks"].items()) {
      int idx = std::stoi(key);
      if (idx >= 0 && idx < 10) {
        std::string hex_str = value.get<std::string>();
        alias_buffer[idx].clear();
        alias_buffer[idx].reserve(hex_str.length() / 2);
        for (size_t i = 0; i + 1 < hex_str.length(); i += 2) {
          uint8_t byte = static_cast<uint8_t>(std::stoul(hex_str.substr(i, 2), nullptr, 16));
          alias_buffer[idx].push_back(byte);
        }
      }
    }
  }
  
  OTAAlias result;
  if (j["type"] == "motorola_alias_p1") {
    result = UnitTagsOTA::decode_motorola_alias(alias_buffer, messages);
  } else if (j["type"] == "motorola_alias_p2") {
    result = UnitTagsOTA::decode_motorola_alias_p2(alias_buffer, messages);
  } else if (j["type"] == "harris_alias_p1") {
    std::string wacn = d_system ? std::to_string(d_system->get_wacn()) : "";
    std::string sys_id = d_system ? std::to_string(d_system->get_sys_id()) : "";
    
    long unit_id = -1;
    long talkgroup = -1;
    
    // Get OP25-captured IDs
    if (j.contains("op25_src_id") && j.contains("op25_grp_id")) {
      unit_id = j["op25_src_id"].get<long>();
      talkgroup = j["op25_grp_id"].get<long>();
    }
    
    // Check cache age - if IDs were cached too long ago, they may be stale
    // This handles the case where message queue processing is delayed
    if (j.contains("cache_age_ms")) {
      long cache_age = j["cache_age_ms"].get<long>();
      long cache_threshold = 300;
      // If cache is older, the IDs may be from a previous transmission
      if (cache_age > cache_threshold) {
        BOOST_LOG_TRIVIAL(debug) << "Harris P1 alias deferred - cache too old (" 
                                  << cache_age << "ms > " << cache_threshold << "ms threshold)";
        return;  // Skip and wait for retransmission with fresher IDs
      }
    }

    // Check that the alias source matches the current call
    long call_src_id = d_source_id;
    if (call_src_id != unit_id) {
      BOOST_LOG_TRIVIAL(debug) << "Harris P1 alias deferred - source id mismatch (OP25 src="
                               << unit_id << ", call ID=" << call_src_id << ")";
      return;
    }

    if (unit_id <= 0 || talkgroup <= 0) {
      BOOST_LOG_TRIVIAL(debug) << "Harris P1 alias deferred - OP25 cache not yet populated (src="
                               << unit_id << ", grp=" << talkgroup << ")";
      return;  // Skip and wait for retransmission with valid cached IDs
    }

    BOOST_LOG_TRIVIAL(debug) << "Harris P1 using OP25-cached IDs: src=" << unit_id << ", grp=" << talkgroup;

    result = UnitTagsOTA::decode_harris_alias(alias_buffer, unit_id, talkgroup, wacn, sys_id);
  } else if (j["type"] == "harris_alias_p2") {
    std::string wacn = d_system ? std::to_string(d_system->get_wacn()) : "";
    std::string sys_id = d_system ? std::to_string(d_system->get_sys_id()) : "";

    long unit_id = -1;
    long talkgroup = -1;

    // Get OP25-captured IDs
    if (j.contains("op25_src_id") && j.contains("op25_grp_id")) {
      unit_id = j["op25_src_id"].get<long>();
      talkgroup = j["op25_grp_id"].get<long>();
    }

    // Check cache age - if IDs were cached too long ago, they may be stale
    // This handles the case where message queue processing is delayed
    if (j.contains("cache_age_ms")) {
      long cache_age = j["cache_age_ms"].get<long>();
      long cache_threshold = 300;
      // If cache is older, the IDs may be from a previous transmission
      if (cache_age > cache_threshold) {
        BOOST_LOG_TRIVIAL(debug) << "Harris P2 alias deferred - cache too old ("
                                  << cache_age << "ms > " << cache_threshold << "ms threshold)";
        return;  // Skip and wait for retransmission with fresher IDs
      }
    }

    // Check that the alias source matches the current call
    long call_src_id = d_source_id;
    if (call_src_id != unit_id) {
      BOOST_LOG_TRIVIAL(debug) << "Harris P2 alias deferred - source id mismatch (OP25 src="
                               << unit_id << ", call ID=" << call_src_id << ")";
      return;
    }
    
    if (unit_id <= 0 || talkgroup <= 0) {
      BOOST_LOG_TRIVIAL(debug) << "Harris P2 alias deferred - OP25 cache not yet populated (src=" 
                               << unit_id << ", grp=" << talkgroup << ")";
      return;  // Skip and wait for retransmission with valid cached IDs
    }
    
    BOOST_LOG_TRIVIAL(debug) << "Harris P2 using OP25-cached IDs: src=" << unit_id << ", grp=" << talkgroup;
    
    result = UnitTagsOTA::decode_harris_alias_p2(alias_buffer, unit_id, talkgroup, wacn, sys_id);
  }
  
  if (result.success && !result.alias.empty()) {
    BOOST_LOG_TRIVIAL(info) << Color::BMAG << "New " << result.source << " alias: " << Color::RST
                            << result.radio_id << " (" << Color::BLU << result.alias << Color::RST << ")";
    if (d_config.event_sink && d_system) {
      d_config.event_sink->unit_alias_discovered(d_system, result);
    }
  }
}

