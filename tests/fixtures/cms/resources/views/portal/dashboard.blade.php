@extends('layouts.app')
@section('content')
  <h1>{{ __('messages.dashboard') }}</h1>
  <p>Welcome back</p>
  @include('partials.profile-form')
@endsection
