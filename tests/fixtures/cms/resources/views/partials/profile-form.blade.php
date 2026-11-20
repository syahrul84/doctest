<form method="POST" action="{{ route('portal.profile.update') }}">
  @csrf
  <label for="phone">@lang('messages.phone')</label>
  <input name="phone">
  <button type="submit">Save profile</button>
</form>
