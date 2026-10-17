
#include "smartnet_impl.h"
#include "../formatter.h"
#include "smartnet_fsk2_demod.h"
#include <boost/log/trivial.hpp>

smartnet_impl::sptr smartnet_impl::make(double freq, double center, const ChannelTap &tap, int sys_num) {
  smartnet_impl *smartnet = new smartnet_impl(freq, center, tap, sys_num);

  return gnuradio::get_initial_sptr(smartnet);
}

smartnet_impl::smartnet_impl(double freq, double center, const ChannelTap &tap, int sys_num)
    : gr::hier_block2("smartnet_impl",
                      gr::io_signature::make(1, 1, sizeof(gr_complex)),
                      gr::io_signature::make(0, 0, sizeof(float)))
{
  initialize(freq, center, tap, sys_num);
}

smartnet_impl::~smartnet_impl() {

}

void smartnet_impl::initialize(double freq, double center, const ChannelTap &tap, int sys_num) {
  chan_freq = freq;
  center_freq = center;
  input_rate = static_cast<long>(tap.rate);
  this->sys_num = sys_num;


  prefilter = xlat_channelizer::make(xlat_channelizer::smartnet_samples_per_symbol, xlat_channelizer::smartnet_symbol_rate, xlat_channelizer::channel_bandwidth, false, tap.rate, xlat_channelizer::smartnet_excess_bw, static_cast<bool>(tap.cond.set));
  prefilter->set_tune_hook(tap.set_offset);
  prefilter->set_filter_hook(tap.set_filter);
  if (prefilter->external_conditioning()) {
    prefilter->set_conditioning_hooks(tap.cond);
  }

  double offset_amount = (center_freq - chan_freq);
  prefilter->tune_offset(offset_amount);
  // initialize_prefilter();
  //  initialize_p25();

  fsk2_demod = smartnet_fsk2_demod::make();

  connect(self(), 0, prefilter, 0);
  connect(prefilter, 0, fsk2_demod, 0);

}



int smartnet_impl::get_freq_error() { // get frequency error from FLL and convert to Hz
  return prefilter->get_freq_error();
}


double smartnet_impl::get_pwr() {
  return prefilter->get_pwr();
}


double smartnet_impl::get_freq() {
  return chan_freq;
}


void smartnet_impl::tune_freq(double f) {
  chan_freq = f;
  float freq = (center_freq - f);
  prefilter->tune_offset(freq);
}

void smartnet_impl::set_center(double c) {
  center_freq = c;
  double offset_amount = (center_freq - chan_freq);
  prefilter->tune_offset(offset_amount);
}

void smartnet_impl::set_rate(long s) {
  input_rate = s;
}

void smartnet_impl::enable() {
  
}

void smartnet_impl::set_msg_callback(std::function<void(gr::message::sptr)> cb) {
  if (fsk2_demod)
    fsk2_demod->set_msg_callback(std::move(cb));
}

void smartnet_impl::finetune_control_freq(double f) {
   tune_freq(f);
}
