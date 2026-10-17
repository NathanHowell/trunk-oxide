/* -*- c++ -*- */
/*
 * CTCSS (PL) tone squelch block with callback support.
 *
 * Tone detection is ported verbatim from gnuradio's ctcss_squelch_ff_impl
 * (three Goertzel correlators: center tone + the two neighbouring standard
 * CTCSS tones). The squelch state machine and callback are the same as
 * the power squelch now in trunk-channelizer's conditioner.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

#include "./callback_ctcss_squelch_ff.h"
#include <gnuradio/fft/goertzel.h>
#include <gnuradio/io_signature.h>
#include <gnuradio/math.h>
#include <cmath>
#include <memory>

namespace {

// Standard CTCSS tones; used to choose neighbouring frequencies for the side
// correlators (verbatim from gnuradio's ctcss_squelch_ff_impl).
const float ctcss_tones[] = { 67.0,  71.9,  74.4,  77.0,  79.7,  82.5,  85.4,  88.5,
                              91.5,  94.8,  97.4,  100.0, 103.5, 107.2, 110.9, 114.8,
                              118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4,
                              156.7, 162.2, 167.9, 173.8, 179.9, 186.2, 192.8, 203.5,
                              210.7, 218.1, 225.7, 233.6, 241.8, 250.3 };
const int max_tone_index = 37;

int find_tone(float freq)
{
    for (int i = 0; i <= max_tone_index; i++)
        if (std::abs(ctcss_tones[i] - freq) < 0.001)
            return i;
    return -1;
}

void compute_freqs(float freq, float& f_l, float& f_r)
{
    int i = find_tone(freq);

    if (i == -1 || i == 0)
        f_l = freq * 0.98f;
    else
        f_l = ctcss_tones[i - 1];

    if (i == -1 || i == max_tone_index)
        f_r = freq * 1.02f;
    else
        f_r = ctcss_tones[i + 1];
}

class callback_ctcss_squelch_ff_impl : public callback_ctcss_squelch_ff
{
private:
    // Tone detection
    float d_freq;
    float d_level;
    int d_rate;
    int d_len;
    bool d_mute;
    double d_last_level;
    std::unique_ptr<gr::fft::goertzel> d_goertzel_l;
    std::unique_ptr<gr::fft::goertzel> d_goertzel_c;
    std::unique_ptr<gr::fft::goertzel> d_goertzel_r;

    // Squelch state machine (mirrors the power squelch in trunk-channelizer)
    int d_ramp;
    int d_ramped;
    bool d_gate;
    double d_envelope;
    enum { ST_MUTED, ST_ATTACK, ST_UNMUTED, ST_DECAY } d_state;

    // Tags + callback
    const pmt::pmt_t d_sob_key;
    const pmt::pmt_t d_eob_key;
    bool d_tag_next_unmuted;
    std::function<void(bool, double)> d_squelch_cb;

    void publish_state(bool muted, double level)
    {
        if (d_squelch_cb)
            d_squelch_cb(muted, level);
    }

protected:
    // Feed one sample to the correlators; refresh d_mute once a full window of
    // d_len samples has been accumulated.
    void update_state(const float& in) override
    {
        d_goertzel_l->input(in);
        d_goertzel_c->input(in);
        d_goertzel_r->input(in);

        if (d_goertzel_c->ready()) {
            const float rounder = 100000.0f;
            float out_l = std::floor(rounder * std::abs(d_goertzel_l->output())) / rounder;
            float out_c = std::floor(rounder * std::abs(d_goertzel_c->output())) / rounder;
            float out_r = std::floor(rounder * std::abs(d_goertzel_r->output())) / rounder;
            d_last_level = out_c;
            d_mute = (out_c < d_level || out_c < out_l || out_c < out_r);
        }
    }

    bool mute() const override { return d_mute; }

public:
    callback_ctcss_squelch_ff_impl(int rate, float freq, float level, int len, int ramp, bool gate)
        : gr::block("callback_ctcss_squelch_ff",
                    gr::io_signature::make(1, 1, sizeof(float)),
                    gr::io_signature::make(1, 1, sizeof(float))),
          d_freq(freq),
          d_level(level),
          d_rate(rate),
          d_mute(true),
          d_last_level(0.0),
          d_ramp(ramp),
          d_ramped(0),
          d_gate(gate),
          d_envelope(ramp ? 0.0 : 1.0),
          d_state(ST_MUTED),
          d_sob_key(pmt::intern("squelch_sob")),
          d_eob_key(pmt::intern("squelch_eob")),
          d_tag_next_unmuted(true)
    {
        // gnuradio default: two cycles' worth at a 10 Hz resolution.
        d_len = (len == 0) ? static_cast<int>(d_rate / 10.0) : len;

        float f_l, f_r;
        compute_freqs(d_freq, f_l, f_r);
        d_goertzel_l = std::make_unique<gr::fft::goertzel>(d_rate, d_len, f_l);
        d_goertzel_c = std::make_unique<gr::fft::goertzel>(d_rate, d_len, d_freq);
        d_goertzel_r = std::make_unique<gr::fft::goertzel>(d_rate, d_len, f_r);
    }

    ~callback_ctcss_squelch_ff_impl() override = default;

    // --- callback_ctcss_squelch_ff interface ---

    void set_squelch_callback(std::function<void(bool, double)> cb) override
    {
        gr::thread::scoped_lock l(d_setlock);
        d_squelch_cb = std::move(cb);
    }

    // --- ctcss_squelch_ff interface ---

    std::vector<float> squelch_range() const override { return { 0.0f, 1.0f, 0.01f }; }

    float level() const override { return d_level; }

    void set_level(float level) override
    {
        gr::thread::scoped_lock l(d_setlock);
        d_level = level;
    }

    int len() const override { return d_len; }

    float frequency() const override { return d_freq; }

    void set_frequency(float frequency) override
    {
        gr::thread::scoped_lock l(d_setlock);
        d_freq = frequency;
        float f_l, f_r;
        compute_freqs(d_freq, f_l, f_r);
        d_goertzel_l->set_params(d_rate, d_len, f_l);
        d_goertzel_c->set_params(d_rate, d_len, d_freq);
        d_goertzel_r->set_params(d_rate, d_len, f_r);
    }

    // --- squelch_base_ff interface ---

    int ramp() const override { return d_ramp; }

    void set_ramp(int ramp) override
    {
        gr::thread::scoped_lock l(d_setlock);
        d_ramp = ramp;
    }

    bool gate() const override { return d_gate; }

    void set_gate(bool gate) override
    {
        gr::thread::scoped_lock l(d_setlock);
        d_gate = gate;
    }

    bool unmuted() const override
    {
        return d_state == ST_UNMUTED || d_state == ST_ATTACK;
    }

    // --- gr::block ---

    int general_work(int noutput_items,
                     gr_vector_int& ninput_items,
                     gr_vector_const_void_star& input_items,
                     gr_vector_void_star& output_items) override
    {
        const auto* in = static_cast<const float*>(input_items[0]);
        auto* out = static_cast<float*>(output_items[0]);

        int j = 0;
        gr::thread::scoped_lock l(d_setlock);

        for (int i = 0; i < noutput_items; i++) {
            update_state(in[i]);

            switch (d_state) {
            case ST_MUTED:
                if (!mute()) {
                    d_state = d_ramp ? ST_ATTACK : ST_UNMUTED;
                    if (d_state == ST_UNMUTED)
                        d_tag_next_unmuted = true;
                }
                break;

            case ST_UNMUTED:
                if (d_tag_next_unmuted) {
                    d_tag_next_unmuted = false;
                    add_item_tag(0, nitems_written(0) + j, d_sob_key, pmt::PMT_NIL);
                    publish_state(false, d_last_level);
                }
                if (mute()) {
                    d_state = d_ramp ? ST_DECAY : ST_MUTED;
                    if (d_state == ST_MUTED) {
                        add_item_tag(0, nitems_written(0) + j, d_eob_key, pmt::PMT_NIL);
                        publish_state(true, d_last_level);
                    }
                }
                break;

            case ST_ATTACK:
                d_envelope = 0.5 - std::cos(GR_M_PI * (++d_ramped) / d_ramp) / 2.0;
                if (d_ramped >= d_ramp) {
                    d_state = ST_UNMUTED;
                    d_tag_next_unmuted = true;
                    d_envelope = 1.0;
                }
                break;

            case ST_DECAY:
                d_envelope = 0.5 - std::cos(GR_M_PI * (--d_ramped) / d_ramp) / 2.0;
                if (d_ramped == 0) {
                    d_state = ST_MUTED;
                    add_item_tag(0, nitems_written(0) + j, d_eob_key, pmt::PMT_NIL);
                    publish_state(true, d_last_level);
                }
                break;
            }

            if (d_state != ST_MUTED) {
                out[j++] = in[i] * static_cast<float>(d_envelope);
            } else if (!d_gate) {
                out[j++] = 0.0f;
            }
        }

        consume_each(noutput_items);
        return j;
    }
};

} // anonymous namespace

callback_ctcss_squelch_ff::sptr
callback_ctcss_squelch_ff::make(int rate, float freq, float level, int len, int ramp, bool gate)
{
    return gnuradio::make_block_sptr<callback_ctcss_squelch_ff_impl>(
        rate, freq, level, len, ramp, gate);
}
