// Local stdin/stdout PCM worker. ABI headers are pinned to bundled whisper.cpp v1.9.2.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <io.h>
#include <fcntl.h>
#include <cstdint>
#include <algorithm>
#include <thread>
#include <iostream>
#include <vector>
#include <string>
#include "include/whisper.h"
static bool packet(uint8_t kind, const std::string& text) {
    uint32_t size=static_cast<uint32_t>(text.size());
    std::cout.write(reinterpret_cast<const char*>(&kind),1);
    std::cout.write(reinterpret_cast<const char*>(&size),4);
    std::cout.write(text.data(),size); std::cout.flush();return bool(std::cout);
}
static void quiet(enum ggml_log_level,const char*,void*){}
// Orthography only: do not supply meeting facts or instructions to the decoder.
static std::string simplified_chinese(const std::string& text) {
    if(text.empty())return text;
    const int size=MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data(),static_cast<int>(text.size()),nullptr,0);
    if(size<=0)return text;
    std::wstring wide(size,L'\0');
    if(!MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,text.data(),static_cast<int>(text.size()),wide.data(),size))return text;
    const int mapped_size=LCMapStringEx(L"zh-CN",LCMAP_SIMPLIFIED_CHINESE,wide.data(),size,nullptr,0,nullptr,nullptr,0);
    if(mapped_size<=0)return text;
    std::wstring mapped(mapped_size,L'\0');
    if(!LCMapStringEx(L"zh-CN",LCMAP_SIMPLIFIED_CHINESE,wide.data(),size,mapped.data(),mapped_size,nullptr,nullptr,0))return text;
    const int bytes=WideCharToMultiByte(CP_UTF8,0,mapped.data(),mapped_size,nullptr,0,nullptr,nullptr);
    if(bytes<=0)return text;
    std::string result(bytes,'\0');
    if(!WideCharToMultiByte(CP_UTF8,0,mapped.data(),mapped_size,result.data(),bytes,nullptr,nullptr))return text;
    return result;
}
int main(){
    _setmode(_fileno(stdin),_O_BINARY);_setmode(_fileno(stdout),_O_BINARY);
    SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_APPLICATION_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
    // Backend discovery is restricted to this application's directory. Clear the
    // upstream override before loading DLLs (their CRT snapshots the environment).
    SetEnvironmentVariableW(L"GGML_BACKEND_PATH",nullptr);
    wchar_t executable[32768];
    const DWORD executable_size=GetModuleFileNameW(nullptr,executable,32768);
    if(!executable_size||executable_size>=32768){packet(2,"audio backend path unavailable");return 4;}
    std::wstring directory(executable,executable_size);
    directory.resize(directory.find_last_of(L"\\"));
    const int path_size=WideCharToMultiByte(CP_UTF8,0,directory.data(),static_cast<int>(directory.size()),nullptr,0,nullptr,nullptr);
    std::string backend_path(path_size,'\0');
    WideCharToMultiByte(CP_UTF8,0,directory.data(),static_cast<int>(directory.size()),backend_path.data(),path_size,nullptr,nullptr);
    auto library=LoadLibraryExW(L"whisper.dll",nullptr,LOAD_LIBRARY_SEARCH_APPLICATION_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
    if(!library){packet(2,"whisper library unavailable");return 1;}
#define API(name) auto p_##name=reinterpret_cast<decltype(&name)>(GetProcAddress(library,#name));if(!p_##name){packet(2,"incompatible whisper library");return 2;}
    API(whisper_log_set) API(whisper_context_default_params) API(whisper_init_from_file_with_params)
    API(whisper_full_default_params) API(whisper_full) API(whisper_full_n_segments) API(whisper_full_get_segment_text) API(whisper_free)
    p_whisper_log_set(quiet,nullptr);
    auto ggml=GetModuleHandleW(L"ggml.dll");
    auto load_backends=reinterpret_cast<decltype(&ggml_backend_load_all_from_path)>(GetProcAddress(ggml,"ggml_backend_load_all_from_path"));
    auto gpu_device=reinterpret_cast<decltype(&ggml_backend_dev_by_type)>(GetProcAddress(ggml,"ggml_backend_dev_by_type"));
    if(!load_backends){packet(2,"audio backend unavailable");return 4;}
    load_backends(backend_path.c_str());
    auto cp=p_whisper_context_default_params();
    bool gpu_active=gpu_device&&gpu_device(GGML_BACKEND_DEVICE_TYPE_GPU)!=nullptr;
    cp.use_gpu=gpu_active;
    auto ctx=p_whisper_init_from_file_with_params("ggml-small-q5_1.bin",cp);
    if(!ctx&&gpu_active){
        gpu_active=false;cp.use_gpu=false;
        ctx=p_whisper_init_from_file_with_params("ggml-small-q5_1.bin",cp);
    }
    std::cerr<<"speech backend: "<<(gpu_active?"gpu":"cpu")<<"\n";
    if(!ctx){packet(2,"speech model unavailable");return 3;}
    if(!packet(0,"ready:2")){p_whisper_free(ctx);return 0;}
    const unsigned processors=std::thread::hardware_concurrency();
    const int threads=processors==0?4:static_cast<int>(std::min(8u,processors));
    uint32_t count=0;
    uint8_t language=0;
    while(std::cin.read(reinterpret_cast<char*>(&language),1)){
        if(!std::cin.read(reinterpret_cast<char*>(&count),4))break;
        if(language>2){packet(2,"invalid audio language");break;}
        if(count==0||count>16000*15){packet(2,"invalid audio length");break;}
        std::vector<int16_t> pcm(count);
        if(!std::cin.read(reinterpret_cast<char*>(pcm.data()),count*2))break;
        // The capture gate needs a frame RMS above 110. If every sample is at or
        // below that level, no such frame exists: do not hallucinate tiny noise.
        const bool above_capture_gate=std::any_of(pcm.begin(),pcm.end(),[](int16_t value){return value>110||value<-110;});
        if(!above_capture_gate){
            if(!packet(1,""))break;
            continue;
        }
        std::vector<float> samples(count);
        for(size_t i=0;i<count;++i)samples[i]=pcm[i]/32768.0f;
        auto params=p_whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
        params.n_threads=threads;params.no_context=true;params.no_timestamps=true;
        params.print_realtime=false;params.print_progress=false;params.print_timestamps=false;params.print_special=false;
        params.language=language==1?"zh":language==2?"en":"auto";params.greedy.best_of=1;
        // Keep the trained context: shorter experimental contexts can omit early speech.
        params.audio_ctx=1500;
        int result=p_whisper_full(ctx,params,samples.data(),static_cast<int>(count));
        // A returned GPU failure is retried once on CPU, then this worker stays
        // on CPU. Crashes/hangs remain bounded by the parent's worker timeout.
        if(result!=0&&gpu_active){
            p_whisper_free(ctx);ctx=nullptr;gpu_active=false;cp.use_gpu=false;
            ctx=p_whisper_init_from_file_with_params("ggml-small-q5_1.bin",cp);
            std::cerr<<"speech backend: cpu (gpu decode fallback)\n";
            if(!ctx){packet(2,"speech model unavailable");break;}
            result=p_whisper_full(ctx,params,samples.data(),static_cast<int>(count));
        }
        if(result!=0){if(!packet(2,"transcription failed"))break;continue;}
        std::string text;
        for(int i=0;i<p_whisper_full_n_segments(ctx);++i){const char* part=p_whisper_full_get_segment_text(ctx,i);if(part)text+=part;}
        if(language!=2)text=simplified_chinese(text);
        if(text.size()>24000)text.resize(24000);
        if(!packet(1,text))break;
    }
    if(ctx)p_whisper_free(ctx);FreeLibrary(library);return 0;
}

