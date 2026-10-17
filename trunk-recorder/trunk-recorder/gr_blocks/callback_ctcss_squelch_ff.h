/* -*- c++ -*- */
/*
 * CTCSS (PL) tone squelch block with callback support.
 *
 * Subclasses gr::analog::ctcss_squelch_ff to add a callback that fires on
 * squelch state transitions, so conventional call boundaries can be driven by
 * PL-tone presence instead of carrier power, without polling.
 *
 * gnuradio does not export the concrete ctcss_squelch_ff_impl, so we subclass
 * the abstract interface and provide our own general_work() and tone detector
 * (mirrors the power squelch in trunk-channelizer).
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

#ifndef CALLBACK_CTCSS_SQUELCH_FF_H
#define CALLBACK_CTCSS_SQUELCH_FF_H

#include <functional>
#include <gnuradio/analog/ctcss_squelch_ff.h>

class callback_ctcss_squelch_ff : public gr::analog::ctcss_squelch_ff
{
public:
    typedef std::shared_ptr<callback_ctcss_squelch_ff> sptr;

    static sptr make(int rate, float freq, float level, int len, int ramp, bool gate);

    // Fires (muted, level) on every squelch state transition. level is the
    // center-tone correlation magnitude at the time of the transition.
    virtual void set_squelch_callback(std::function<void(bool, double)> cb) = 0;
};

#endif /* CALLBACK_CTCSS_SQUELCH_FF_H */
