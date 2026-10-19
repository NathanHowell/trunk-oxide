#ifndef P25_TRUNKING_H
#define P25_TRUNKING_H

#define _USE_MATH_DEFINES

#include <cstdio>
#include <functional>
#include <iostream>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#include <boost/log/trivial.hpp>

#include <gnuradio/hier_block2.h>
#include <gnuradio/io_signature.h>

#include <gnuradio/block.h>

#include <gnuradio/blocks/null_sink.h>
#include <gnuradio/message.h>
#include <gnuradio/msg_queue.h>

#include "../gr_blocks/channel_control.h"
#include "trunking_decoder.h"

class p25_trunking;

typedef std::shared_ptr<p25_trunking> p25_trunking_sptr;

p25_trunking_sptr make_p25_trunking(double f,
                                    double c,
                                    const ChannelTap &tap,
                                    bool qpsk,
                                    int sys_num);

class p25_trunking : public gr::hier_block2, public trunking_decoder {
  friend p25_trunking_sptr make_p25_trunking(double f,
                                             double c,
                                             const ChannelTap &tap,
                                             bool qpsk,
                                             int sys_num);

protected:
  p25_trunking(double f,
               double c,
               const ChannelTap &tap,
               bool qpsk,
               int sys_num);

public:
  ~p25_trunking();

  void set_center(double c);
  void set_rate(long s);
  void tune_freq(double f) override;
  double get_freq() override;
  void enable();
  double get_pwr() override;
  int get_freq_error() override;
  void finetune_control_freq(double f) override;
  void set_msg_callback(std::function<void(gr::message::sptr)> cb) override;
  int autotune_offset;

  // TrunkingDecoder interface
  int get_autotune_offset() const override { return autotune_offset; }
  void set_autotune_offset(int offset) override { autotune_offset = offset; }
  std::shared_ptr<gr::hier_block2> as_hier_block() override { return std::dynamic_pointer_cast<gr::hier_block2>(shared_from_this()); }

private:
  double center_freq, chan_freq;
  long input_rate;
  bool qpsk_mod;
  int sys_num;

  channel_control::sptr prefilter;
  gr::blocks::null_sink::sptr null_sink;
};

#endif // ifndef P25_TRUNKING_H
