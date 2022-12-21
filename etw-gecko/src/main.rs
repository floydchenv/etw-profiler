use std::{collections::{HashMap, HashSet, hash_map::Entry, BTreeMap}, convert::TryInto, fs::File, io::{BufWriter}, path::{Path, PathBuf}, time::{Duration, Instant, SystemTime}};

use etw_reader::{GUID, open_trace, parser::{Parser, TryParse, Address}, print_property, schema::SchemaLocator, write_property};
use serde_json::{Value, json, to_writer};

use gecko_profile::{MarkerDynamicField, MarkerFieldFormat, MarkerLocation, MarkerSchema, MarkerSchemaField, MarkerTiming, ProfilerMarker, TextMarker, ThreadBuilder, debugid};
use debugid::DebugId;
use uuid::Uuid;

use std::collections::Bound::{Included, Unbounded};

fn is_kernel_address(ip: u64, pointer_size: u32) -> bool {
    if pointer_size == 4 {
        return ip >= 0x80000000;
    }
    return ip >= 0xFFFF000000000000;        // TODO I don't know what the true cutoff is.
}
struct ThreadState {
    builder: ThreadBuilder,
    merge_name: Option<String>,
    last_kernel_stack: Option<Vec<u64>>,
    last_kernel_stack_time: u64,
    last_sample_timestamp: Option<i64>,
    running_since_time: Option<i64>,
    total_running_time: i64,
    previous_sample_cpu_time: i64,
}

impl ThreadState {
    fn new(builder: ThreadBuilder) -> Self {
        ThreadState {
            builder,
            last_kernel_stack: None,
            last_kernel_stack_time: 0,
            last_sample_timestamp: None,
            merge_name: None,
            running_since_time: None,
            previous_sample_cpu_time: 0,
            total_running_time: 0
        }
    }
}


fn strip_thread_numbers(name: &str) -> &str {
    if let Some(hash) = name.find('#') {
        let (prefix, suffix) = name.split_at(hash);
        if suffix[1..].parse::<i32>().is_ok() {
            return prefix.trim();
        }
    }
    return name;
}

fn main() {
    let profile_start_instant = Instant::now();
    let profile_start_system = SystemTime::now();

    let mut schema_locator = SchemaLocator::new();
    etw_reader::add_custom_schemas(&mut schema_locator);
    let mut threads: HashMap<u32, ThreadState> = HashMap::new();
    let mut libs: HashMap<u64, (PathBuf, u32)> = HashMap::new();
    let start = Instant::now();
    let mut pargs = pico_args::Arguments::from_env();
    let merge_threads = pargs.contains("--merge-threads");
    let include_idle = pargs.contains("--idle");

    let trace_file: String = pargs.free_from_str().unwrap();

    let mut process_targets = HashSet::new();
    let mut process_target_name = None;
    if let Ok(process_filter) = pargs.free_from_str::<String>() {
        if let Ok(process_id) = process_filter.parse() {
            process_targets.insert(process_id);
        } else {
            println!("targeting {}", process_filter);
            process_target_name = Some(process_filter);
        }
    } else {
        println!("No process specified");
        std::process::exit(1);
    }
    
    let command_name = process_target_name.as_deref().unwrap_or("firefox");
    let mut profile = gecko_profile::ProfileBuilder::new(profile_start_instant, profile_start_system, command_name, 34, Duration::from_secs_f32(1. / 8192.));

    let mut thread_index = 0;
    let mut sample_count = 0;
    let mut stack_sample_count = 0;
    let mut dropped_sample_count = 0;
    let mut timer_resolution: u32 = 0; // Resolution of the hardware timer, in units of 100 nanoseconds.
    let mut start_time: u64 = 0;
    let mut perf_freq: u64 = 0;
    let mut event_count = 0;
    let mut global_thread = ThreadBuilder::new(1, 1, profile_start_instant, false, false);
    let mut gpu_thread = ThreadBuilder::new(1, 1, profile_start_instant, true, false);
    let mut has_vsync = false;
    let mut jscript_symbols: HashMap<u32, BTreeMap<u64, (u64, String)>> = HashMap::new();
    let mut jscript_sources: HashMap<u64, String> = HashMap::new();

    open_trace(Path::new(&trace_file), |e| {
        event_count += 1;
        let s = schema_locator.event_schema(e);
        if let Ok(s) = s {
            let _to_millis = |timestamp: i64| {
                (timestamp as f64 / perf_freq as f64) * 1000.
            };
            // XXX: be careful with this as it can overflow
            let to_nanos = |timestamp: u64| {
                timestamp * 1000 * 1000 * 1000 / perf_freq 
            };
            match s.name() {
                "MSNT_SystemTrace/EventTrace/Header" => {
                    let mut parser = Parser::create(&s);
                    timer_resolution = parser.parse("TimerResolution");
                    perf_freq = parser.parse("PerfFreq");

                    start_time = e.EventHeader.TimeStamp as u64;

                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        print_property(&mut parser, &property);
                    }
                }
                "MSNT_SystemTrace/PerfInfo/CollectionStart" => {
                    let mut parser = Parser::create(&s);
                    let interval: u32 = parser.parse("NewInterval");
                    let interval = Duration::from_nanos(interval as u64 * 100);
                    println!("Sample rate {}ms", interval.as_secs_f32() * 1000.);
                    profile.set_interval(interval);
                }
                "MSNT_SystemTrace/Thread/SetName" => {
                    let mut parser = Parser::create(&s);

                    let process_id: u32 = parser.parse("ProcessId");
                    let thread_id: u32 = parser.parse("ThreadId");
                    let thread_name: String = parser.parse("ThreadName");
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let thread_start_instant = profile_start_instant;
                            let tb = e.insert(
                                ThreadState::new(ThreadBuilder::new(process_id, thread_index, thread_start_instant, false, false))
                            );
                            thread_index += 1;
                            tb
                         }
                    };
                    thread.builder.set_name(&thread_name);
                    thread.merge_name = Some(thread_name);
                }
                "MSNT_SystemTrace/Thread/Start" |
                "MSNT_SystemTrace/Thread/DCStart" => {
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("TThreadId");
                    let process_id: u32 = parser.parse("ProcessId");
                    //assert_eq!(process_id,s.process_id());
                    //println!("thread_name pid: {} tid: {} name: {:?}", process_id, thread_id, thread_name);

                    if !process_targets.contains(&process_id) {
                        return;
                    }

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let thread_start_instant = profile_start_instant;
                            let tb = e.insert(
                                ThreadState::new(ThreadBuilder::new(process_id, thread_index, thread_start_instant, false, false))
                            );
                            thread_index += 1;
                            tb
                        }
                    };

                    let thread_name: Result<String, _> = parser.try_parse("ThreadName");
                    match thread_name {
                        Ok(thread_name) if !thread_name.is_empty() => { thread.builder.set_name(&thread_name); thread.merge_name = Some(thread_name)},
                        _ => {}
                    }
                }
                "MSNT_SystemTrace/Process/Start" |
                "MSNT_SystemTrace/Process/DCStart" => {
                    if let Some(process_target_name) = &process_target_name {
                        let mut parser = Parser::create(&s);


                        let image_file_name: String = parser.parse("ImageFileName");
                        println!("process start {}", image_file_name);

                        let process_id: u32 = parser.parse("ProcessId");
                        if image_file_name.contains(process_target_name) {
                            println!("tracing {}", process_id);
                            process_targets.insert(process_id);
                        }
                    }
                }
                "MSNT_SystemTrace/StackWalk/Stack" => {
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("StackThread");
                    let process_id: u32 = parser.parse("StackProcess");
                    if !process_targets.contains(&process_id) {
                        return;
                    }
                    
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let thread_start_instant = profile_start_instant;
                            let tb = e.insert(
                                ThreadState {
                                    builder: ThreadBuilder::new(process_id, thread_index, thread_start_instant, false, false),
                                    last_kernel_stack: None,
                                    last_kernel_stack_time: 0,
                                    last_sample_timestamp: None,
                                    merge_name: None,
                                    running_since_time: None,
                                    previous_sample_cpu_time: 0,
                                    total_running_time: 0,
                                }
                            );
                            thread_index += 1;
                            tb
                        }
                    };
                    let timestamp: u64 = parser.parse("EventTimeStamp");
                   // eprint!("{} {} {}", thread_id, e.EventHeader.TimeStamp, timestamp);

                    // Only add callstacks if this stack is associated with a SampleProf event
                    if let Some(last) = thread.last_sample_timestamp {
                        if timestamp as i64 != last {
                            //eprintln!("");
                            return
                        }
                    } else {
                        //eprintln!("");
                        return
                    }
                    //eprintln!(" sample");

                    // read the stacks out manually
                    let mut stack = parser.buffer.chunks_exact(8)
                    .map(|a| u64::from_ne_bytes(a.try_into().unwrap()))
                    .collect::<Vec<u64>>();
                    /*
                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        print_property(&mut parser, &property);
                    }*/
                    stack.reverse();

                    let mut add_sample = |thread: &mut ThreadState, timestamp, stack: Vec<u64>| {
                        let mut builder = &mut thread.builder;
                        let frames = stack.iter().map(|addr| {
                            //if process_id == 6736 { dbg!(process_id, jscript_symbols.values()); };

                            if let Some(syms) = jscript_symbols.get(&process_id) {
                                if let Some(sym) = syms.range((Unbounded, Included(addr))).last() {
                                    //if process_id == 6736 { eprintln!("{:x} {:x}", addr, sym.0); }
                                    if *addr < *sym.0 + sym.1.0 {
                                        //eprintln!("found match for {} {:?}", addr, sym);
                                        return gecko_profile::Frame::Label(builder.intern_string(&sym.1.1));
                                    }
                                }
                            }
                            gecko_profile::Frame::Address(*addr)

                        }).collect::<Vec<_>>();
                        if merge_threads {
                            let stack_frames = frames;
                            let mut frames = Vec::new();
                            let thread_name = thread.merge_name.as_ref().map(|x| strip_thread_numbers(x).to_owned()).unwrap_or_else(|| format!("thread {}", thread.builder.get_tid()));
                            frames.push(gecko_profile::Frame::Label(global_thread.intern_string(&thread_name)));
                            frames.extend(stack_frames);
                            global_thread.add_sample(timestamp, frames.into_iter(), Duration::ZERO);
                        } else {
                            let delta = thread.total_running_time - thread.previous_sample_cpu_time;
                            thread.previous_sample_cpu_time = thread.total_running_time;
                            let delta = Duration::from_nanos(to_nanos(delta as u64));
                            thread.builder.add_sample(timestamp, frames.into_iter(), delta);
                        }
                    };

                    if is_kernel_address(stack[0], 8) {
                        //eprintln!("kernel ");
                        thread.last_kernel_stack_time = timestamp;
                        thread.last_kernel_stack = Some(stack);
                    } else {
                        if timestamp == thread.last_kernel_stack_time {
                            //eprintln!("matched");
                            if thread.last_kernel_stack.is_none() {
                                dbg!(thread.last_kernel_stack_time);
                            }
                            let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(timestamp - start_time));
                            stack.append(&mut thread.last_kernel_stack.take().unwrap());
                            add_sample(thread, timestamp, stack);
                        } else {
                            if let Some(kernel_stack) = thread.last_kernel_stack.take() {
                                // we're left with an unassociated kernel stack
                                dbg!(thread.last_kernel_stack_time);

                                let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(thread.last_kernel_stack_time - start_time));
                                add_sample(thread, timestamp, kernel_stack);
                            }
                            let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(timestamp - start_time));
                            add_sample(thread, timestamp, stack);
                        }
                        stack_sample_count += 1;
                        //XXX: what unit are timestamps in the trace in?
                    }
                }
                "MSNT_SystemTrace/PerfInfo/SampleProf" => {
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("ThreadId");
                    //println!("sample {}", thread_id);
                    sample_count += 1;

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(), 
                        Entry::Vacant(_) => {
                            if include_idle && merge_threads {
                                let mut frames = Vec::new();
                                let thread_name = match thread_id {
                                    0 => "Idle",
                                    _ => "Other"
                                };
                                let timestamp = e.EventHeader.TimeStamp as u64;
                                let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(timestamp - start_time));

                                frames.push(gecko_profile::Frame::Label(global_thread.intern_string(&thread_name)));
                                global_thread.add_sample(timestamp, frames.into_iter(), Duration::ZERO);
                            }
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    // assert!(thread.running_since_time.is_some(), "thread {} not running @ {} on {}", thread_id, e.EventHeader.TimeStamp, unsafe { e.BufferContext.Anonymous.ProcessorIndex });
                    thread.last_sample_timestamp = Some(e.EventHeader.TimeStamp);
                }
                "KernelTraceControl/ImageID/" => {

                    let process_id = s.process_id();
                    if !process_targets.contains(&process_id) && process_id != 0 {
                        return;
                    }
                    let mut parser = Parser::create(&s);

                    let image_base: u64 = parser.try_parse("ImageBase").unwrap();
                    // TODO: get the image timestamp and create the CodeId
                    let image_size: u32 = parser.try_parse("ImageSize").unwrap();
                    let binary_path: String = parser.try_parse("OriginalFileName").unwrap();
                    let path = PathBuf::from(binary_path);
                    libs.insert(image_base, (path, image_size));
                }
                "KernelTraceControl/ImageID/DbgID_RSDS" => {
                    let mut parser = Parser::create(&s);

                    let process_id = s.process_id();
                    if !process_targets.contains(&process_id) && process_id != 0 {
                        return;
                    }
                    let image_base: u64 = parser.try_parse("ImageBase").unwrap();

                    let guid: GUID = parser.try_parse("GuidSig").unwrap();
                    let age: u32 = parser.try_parse("Age").unwrap();
                    let debug_id = DebugId::from_parts(Uuid::from_fields(guid.data1, guid.data2, guid.data3, &guid.data4).unwrap(), age);
                    let pdb_path: String = parser.try_parse("PdbFileName").unwrap();
                    let pdb_path = Path::new(&pdb_path);
                    let (ref path, image_size) = libs[&image_base];
                    profile.add_lib(&path, None, &pdb_path, debug_id, Some("x86_64"), image_base, image_base..(image_base + image_size as u64))
                }
                "Microsoft-Windows-DxgKrnl/VSyncDPC/Info " => {
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(timestamp - start_time));
                    has_vsync = true;
                
                    #[derive(Debug, Clone)]
                    pub struct VSyncMarker;

                    impl ProfilerMarker for VSyncMarker {
                        const MARKER_TYPE_NAME: &'static str = "Vsync";

                        fn json_marker_data(&self) -> Value {
                            json!({
                                "type": Self::MARKER_TYPE_NAME,
                                "name": ""
                            })
                        }

                        fn schema() -> MarkerSchema {
                            MarkerSchema {
                                type_name: Self::MARKER_TYPE_NAME,
                                locations: vec![MarkerLocation::MarkerChart, MarkerLocation::MarkerTable, MarkerLocation::TimelineOverview],
                                chart_label: Some("{marker.data.name}"),
                                tooltip_label: None,
                                table_label: Some("{marker.name} - {marker.data.name}"),
                                fields: vec![MarkerSchemaField::Dynamic(MarkerDynamicField {
                                    key: "name",
                                    label: "Details",
                                    format: MarkerFieldFormat::String,
                                    searchable: None,
                                })],
                            }
                        }
                    }
                    gpu_thread.add_marker(
                        "Vsync",
                        VSyncMarker{},
                        MarkerTiming::Instant(timestamp)
                    );
                }
                "MSNT_SystemTrace/Thread/CSwitch" => {
                    let mut parser = Parser::create(&s);
                    let new_thread: u32 = parser.parse("NewThreadId");
                    let old_thread: u32 = parser.parse("OldThreadId");
                    // println!("CSwitch {} -> {} @ {} on {}", old_thread, new_thread, e.EventHeader.TimeStamp, unsafe { e.BufferContext.Anonymous.ProcessorIndex });
                    if let Some(new_thread) = threads.get_mut(&new_thread) {
                        new_thread.running_since_time = Some(e.EventHeader.TimeStamp);
                    };
                    if let Some(old_thread) = threads.get_mut(&old_thread) {
                        if let Some(start_time) = old_thread.running_since_time {
                            old_thread.total_running_time += e.EventHeader.TimeStamp - start_time
                        }
                        old_thread.running_since_time = None;
                    };

                }
                "MSNT_SystemTrace/Thread/ReadyThread" => {
                    // these events can give us the unblocking stack
                    let mut parser = Parser::create(&s);
                    let _thread_id: u32 = parser.parse("TThreadId");
                }
                "V8.js/MethodLoad/" /*|
                "Microsoft-JScript/MethodRuntime/MethodDCStart" |
                "Microsoft-JScript/MethodRuntime/MethodLoad"*/ => {
                    // these events can give us the unblocking stack
                    let mut parser = Parser::create(&s);
                    let method_name: String = parser.parse("MethodName");
                    let method_start_address: Address = parser.parse("MethodStartAddress");
                    let method_size: u64 = parser.parse("MethodSize");
                    let source_id: u64 = parser.parse("SourceID");
                    //if s.process_id() == 6736 { dbg!(s.process_id(), &method_name, method_start_address, method_size); }
                    let syms =  jscript_symbols.entry(s.process_id()).or_insert(BTreeMap::new());
                    let start_address = method_start_address.as_u64();
                    let name_and_file = format!("{} {}", method_name, jscript_sources.get(&source_id).map(|x| x.as_ref()).unwrap_or("?"));

                    let mut overlaps = Vec::new();
                    for sym in syms.range_mut((Included(start_address), Included(start_address + method_size))) {
                        if name_and_file != sym.1.1 || start_address != *sym.0 || method_size != sym.1.0 {
                            println!("overlap {} {} {} -  {:?}", method_name, start_address, method_size, sym);
                            overlaps.push(*sym.0);
                        } else {
                            println!("overlap same {} {} {} -  {:?}", method_name, start_address, method_size, sym);
                        }
                    }
                    for sym in overlaps {
                        syms.remove(&sym);
                    }

                    syms.insert(start_address, (method_size, name_and_file));
                    //dbg!(s.process_id(), jscript_symbols.keys());

                }
                "V8.js/SourceLoad/" /*|
                "Microsoft-JScript/MethodRuntime/MethodDCStart" |
                "Microsoft-JScript/MethodRuntime/MethodLoad"*/ => {
                    // these events can give us the unblocking stack
                    let mut parser = Parser::create(&s);
                    let source_id: u64 = parser.parse("SourceID");
                    let url: String = parser.parse("Url");
                    //if s.process_id() == 6736 { dbg!(s.process_id(), &method_name, method_start_address, method_size); }
                    jscript_sources.insert(source_id, url);
                    //dbg!(s.process_id(), jscript_symbols.keys());

                }
                _ => {
                    if s.name().starts_with("Google.Chrome/") {
                        let mut parser = Parser::create(&s);
                        let timestamp = e.EventHeader.TimeStamp as u64;
                        let timestamp = profile_start_instant + Duration::from_nanos(to_nanos(timestamp - start_time));
                        let thread_id = e.EventHeader.ThreadId;
                        let phase: String = parser.try_parse("Phase").unwrap();
                        let thread = match threads.entry(thread_id) {
                            Entry::Occupied(e) => e.into_mut(), 
                            Entry::Vacant(_) => {
                                dropped_sample_count += 1;
                                // We don't know what process this will before so just drop it for now
                                return;
                            }
                        };
                        let timing = match phase.as_str() {
                            "Complete" => MarkerTiming::IntervalStart(timestamp),
                            "Complete End" => MarkerTiming::IntervalEnd(timestamp),
                            _ => MarkerTiming::Instant(timestamp),
                        };

                        let mut text = String::new();
                        for i in 0..s.property_count() {
                            let property = s.property(i);
                            //dbg!(&property);
                            write_property(&mut text, &mut parser, &property);
                            text += ", "
                        }

                        thread.builder.add_marker(s.name().trim_start_matches("Google.Chrome/"), TextMarker(text), timing)
                    }
                     //println!("unhandled {}", s.name()) 
                    }
            }
            //println!("{}", name);
        }
    });

    if merge_threads {
        profile.add_thread(global_thread);
    } else {
        for (_, thread) in threads.drain() { profile.add_thread(thread.builder); }
    }
    if has_vsync {
        profile.add_thread(gpu_thread);
    }
    let f = File::create("gecko.json").unwrap();
    to_writer(BufWriter::new(f), &profile.to_json()).unwrap();
    println!("Took {} seconds", (Instant::now()-start).as_secs_f32());
    println!("{} events, {} samples, {} dropped, {} stack-samples", event_count, sample_count, dropped_sample_count, stack_sample_count);
}
