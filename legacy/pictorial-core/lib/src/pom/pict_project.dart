import 'package:pictorial_core/pictorial_core.dart';
import 'package:pictorial_core/src/pom/project.dart';

class PictProject extends Project {
  List<Page> _pages = [];
  String _name;

  PictProject(this._name, this._pages);

  String get name => _name;

  List<Page> get pages => _pages;

  int _currentPageId = 0;

  int get currentPageId => _currentPageId;

  Page? getCurrentPage(int? pageId) {
    if (pageId != null && pageId < _pages.length) {
      _currentPageId = pageId;
      return _pages[pageId];
    }
    return null;
  }

  void addPage(Page page) {
    _pages.add(page);
  }

  void removePage(int pageId) {
    _pages.removeWhere((page) => page.id == pageId);
  }

  // To Json
  Map<String, dynamic> toJson() {
    return {
      'name': _name,
      'pages': _pages.map((page) => page.toJson()).toList(),
    };
  }
}
